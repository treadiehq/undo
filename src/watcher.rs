use anyhow::Result;
use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use crate::db::Database;
use crate::ignore::should_ignore;
use crate::models::WatchedProject;
use crate::snapshots;

const DEBOUNCE_MS: u64 = 500;
/// How often to evict stale debounce entries (seconds).
const DEBOUNCE_CLEANUP_SECS: u64 = 60;
/// Entries older than this are eligible for eviction.
const DEBOUNCE_MAX_AGE: Duration = Duration::from_secs(300);
/// Abort initial scan if more files than this are found (unless --force).
pub const MAX_FILES_DEFAULT: usize = 50_000;
/// Timeout for individual filesystem operations (reads, metadata checks).
const FS_TIMEOUT: Duration = Duration::from_secs(5);

// ── fs watchdog ─────────────────────────────────────────────────────

/// Run a filesystem operation on a separate thread with a timeout.
/// Returns None if the operation hangs beyond `FS_TIMEOUT`.
fn fs_with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(FS_TIMEOUT).ok()
}

// ── hashing ─────────────────────────────────────────────────────────

fn compute_hash(data: &[u8]) -> String {
    let result = Sha256::digest(data);
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

// ── debouncer ───────────────────────────────────────────────────────

struct Debouncer {
    last_event: HashMap<PathBuf, Instant>,
    last_cleanup: Instant,
}

impl Debouncer {
    fn new() -> Self {
        Self {
            last_event: HashMap::new(),
            last_cleanup: Instant::now(),
        }
    }

    fn should_process(&mut self, path: &Path) -> bool {
        self.maybe_cleanup();
        let now = Instant::now();
        if let Some(last) = self.last_event.get(path) {
            if now.duration_since(*last) < Duration::from_millis(DEBOUNCE_MS) {
                return false;
            }
        }
        self.last_event.insert(path.to_path_buf(), now);
        true
    }

    /// Periodically evict entries that are too old to matter for debouncing.
    fn maybe_cleanup(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_cleanup)
            < Duration::from_secs(DEBOUNCE_CLEANUP_SECS)
        {
            return;
        }
        self.last_event.retain(|_, t| now.duration_since(*t) < DEBOUNCE_MAX_AGE);
        self.last_cleanup = now;
    }
}

// ── initial scan ────────────────────────────────────────────────────

pub fn initial_scan(
    db: &Database,
    project: &WatchedProject,
    root: &Path,
    verbose: bool,
    force: bool,
) -> Result<()> {
    let max_files = if force { usize::MAX } else { MAX_FILES_DEFAULT };
    initial_scan_with_limit(db, project, root, verbose, max_files)
}

fn initial_scan_with_limit(
    db: &Database,
    project: &WatchedProject,
    root: &Path,
    verbose: bool,
    max_files: usize,
) -> Result<()> {
    let existing_states = db.get_all_file_states(project.id)?;
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut count = 0usize;
    let mut total_files = 0usize;

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !should_ignore(e.path(), root))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() || entry.path_is_symlink() {
            continue;
        }

        total_files += 1;
        if total_files > max_files {
            anyhow::bail!(
                "directory contains more than {} files — this looks too large to watch safely.\n\
                 Use --force to override this limit.",
                max_files
            );
        }

        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();
        seen_paths.insert(path_str.clone());

        let content = match read_if_within_limit(path) {
            Some(c) => c,
            None => continue,
        };

        let hash = compute_hash(&content);
        let existing = db.get_file_state(project.id, &path_str)?;

        match existing {
            Some(ref state) if state.latest_hash.as_deref() == Some(hash.as_str()) => {
                db.upsert_file_state(project.id, &path_str, &hash, true)?;
            }
            Some(ref state) => {
                let prev_hash = state.latest_hash.as_deref();
                let snap = Some(snapshots::save(project.id, &hash, &content)?);
                db.insert_event(
                    project.id,
                    &path_str,
                    "MODIFIED",
                    Some(&hash),
                    prev_hash,
                    snap.as_deref(),
                    None,
                    Some(content.len() as i64),
                )?;
                db.upsert_file_state(project.id, &path_str, &hash, true)?;
                count += 1;
                if verbose {
                    eprintln!(
                        "  scan: MODIFIED {}",
                        crate::relative_path(&path_str, &project.root_path)
                    );
                }
            }
            None => {
                let snap = Some(snapshots::save(project.id, &hash, &content)?);
                db.insert_event(
                    project.id,
                    &path_str,
                    "CREATED",
                    Some(&hash),
                    None,
                    snap.as_deref(),
                    None,
                    Some(content.len() as i64),
                )?;
                db.upsert_file_state(project.id, &path_str, &hash, true)?;
                count += 1;
            }
        }
    }

    // Detect deletions that happened while the daemon was stopped.
    for state in &existing_states {
        if state.exists_now && !seen_paths.contains(&state.path) {
            db.insert_event(
                project.id,
                &state.path,
                "DELETED",
                None,
                state.latest_hash.as_deref(),
                None,
                None,
                None,
            )?;
            db.mark_deleted(project.id, &state.path)?;
            count += 1;
            if verbose {
                eprintln!(
                    "  scan: DELETED {}",
                    crate::relative_path(&state.path, &project.root_path)
                );
            }
        }
    }

    if count > 0 {
        eprintln!("Initial scan: {} change(s) detected.", count);
    }

    Ok(())
}

// ── live watcher ────────────────────────────────────────────────────

/// How often to verify the watched root directory is still accessible.
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How often to auto-prune old history.
const AUTO_PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

fn root_is_accessible(root: &Path) -> bool {
    root.try_exists().unwrap_or(false) && root.is_dir()
}

pub fn watch_directory(
    db: &Database,
    project: &WatchedProject,
    root: &Path,
    shutdown: Arc<AtomicBool>,
    verbose: bool,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;

    watcher.watch(root, RecursiveMode::Recursive)?;

    let mut debouncer = Debouncer::new();
    let mut paused = false;
    let mut last_health_check = Instant::now();
    let mut last_prune = Instant::now();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Periodic health check on the root directory.
        if last_health_check.elapsed() >= HEALTH_CHECK_INTERVAL {
            last_health_check = Instant::now();
            let accessible = root_is_accessible(root);

            if !accessible && !paused {
                eprintln!(
                    "warning: watched directory is no longer accessible — pausing recording"
                );
                paused = true;
            } else if accessible && paused {
                eprintln!("watched directory is accessible again — resuming");
                if let Err(e) = initial_scan(db, project, root, verbose, true) {
                    eprintln!(
                        "{}warning:{} reconciliation scan failed: {}",
                        crate::YELLOW, crate::RESET, e
                    );
                }
                paused = false;
            }
        }

        // Hourly auto-prune.
        if last_prune.elapsed() >= AUTO_PRUNE_INTERVAL {
            last_prune = Instant::now();
            let cfg = crate::retention::load_config(Some(root));
            match crate::retention::prune(db, project.id, &cfg, false) {
                Ok(stats)
                    if stats.events_deleted + stats.snapshots_deleted + stats.backups_deleted
                        > 0 =>
                {
                    eprintln!(
                        "{}auto-prune:{} removed {} events, {} snapshots, {} backups (freed {})",
                        crate::YELLOW, crate::RESET,
                        stats.events_deleted, stats.snapshots_deleted, stats.backups_deleted,
                        crate::retention::format_size(stats.bytes_freed),
                    );
                }
                Err(e) => eprintln!(
                    "{}warning:{} auto-prune failed: {}",
                    crate::YELLOW, crate::RESET, e
                ),
                _ => {}
            }
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                if paused {
                    continue;
                }
                if let Err(e) =
                    process_event(db, project, root, event, &mut debouncer, verbose)
                {
                    // Always surface errors — a silent failure means the user
                    // believes changes are being recorded when they aren't.
                    eprintln!(
                        "{}warning:{} failed to record event: {}",
                        crate::YELLOW, crate::RESET, e
                    );
                }
            }
            Ok(Err(e)) => {
                // Always surface watcher errors too.
                eprintln!(
                    "{}warning:{} file watcher error: {}",
                    crate::YELLOW, crate::RESET, e
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

// ── event dispatch ──────────────────────────────────────────────────

fn process_event(
    db: &Database,
    project: &WatchedProject,
    root: &Path,
    event: Event,
    debouncer: &mut Debouncer,
    verbose: bool,
) -> Result<()> {
    match event.kind {
        EventKind::Create(_) => {
            for path in &event.paths {
                if should_ignore(path, root) || !path.is_file() {
                    continue;
                }
                if debouncer.should_process(path) {
                    handle_create(db, project, path, verbose)?;
                }
            }
        }

        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any) => {
            for path in &event.paths {
                if should_ignore(path, root) || !path.is_file() {
                    continue;
                }
                if debouncer.should_process(path) {
                    handle_modify(db, project, path, verbose)?;
                }
            }
        }

        EventKind::Remove(_) => {
            if !root_is_accessible(root) {
                return Ok(());
            }
            for path in &event.paths {
                if should_ignore(path, root) {
                    continue;
                }
                if debouncer.should_process(path) {
                    handle_delete(db, project, path, verbose)?;
                }
            }
        }

        EventKind::Modify(ModifyKind::Name(_)) => {
            if event.paths.len() >= 2 {
                let old = &event.paths[0];
                let new = &event.paths[1];
                if should_ignore(new, root) {
                    if !should_ignore(old, root) && debouncer.should_process(old) {
                        handle_delete(db, project, old, verbose)?;
                    }
                } else if debouncer.should_process(new) {
                    handle_rename(db, project, old, new, verbose)?;
                }
            } else {
                for path in &event.paths {
                    if should_ignore(path, root) {
                        continue;
                    }
                    if path.exists() && path.is_file() {
                        if debouncer.should_process(path) {
                            handle_modify(db, project, path, verbose)?;
                        }
                    } else if debouncer.should_process(path) {
                        handle_delete(db, project, path, verbose)?;
                    }
                }
            }
        }

        _ => {}
    }

    Ok(())
}

// ── per-event handlers ──────────────────────────────────────────────

/// Read a file only if its on-disk size is within `MAX_SNAPSHOT_SIZE`.
/// Returns None for files that are too large or if the read times out.
fn read_if_within_limit(path: &Path) -> Option<Vec<u8>> {
    let p = path.to_path_buf();
    fs_with_timeout(move || {
        let meta = std::fs::metadata(&p).ok()?;
        if meta.len() > snapshots::MAX_SNAPSHOT_SIZE as u64 {
            return None;
        }
        std::fs::read(&p).ok()
    })?
}

/// Returns true if the path is a symlink (not a regular file).
fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn handle_create(
    db: &Database,
    project: &WatchedProject,
    path: &Path,
    verbose: bool,
) -> Result<()> {
    if is_symlink(path) {
        return Ok(());
    }
    let content = match read_if_within_limit(path) {
        Some(c) => c,
        None => return Ok(()),
    };
    let hash = compute_hash(&content);
    let path_str = path.to_string_lossy().to_string();

    let state = db.get_file_state(project.id, &path_str)?;

    if let Some(ref s) = state {
        if s.latest_hash.as_deref() == Some(&hash) && s.exists_now {
            return Ok(());
        }
    }

    let snap = Some(snapshots::save(project.id, &hash, &content)?);

    // macOS FSEvents can report overwrites as CREATE events.
    // If the file is already tracked and alive, record MODIFIED instead.
    let (event_type, prev_hash) = match &state {
        Some(s) if s.exists_now => ("MODIFIED", s.latest_hash.clone()),
        _ => ("CREATED", None),
    };

    db.insert_event(
        project.id,
        &path_str,
        event_type,
        Some(&hash),
        prev_hash.as_deref(),
        snap.as_deref(),
        None,
        Some(content.len() as i64),
    )?;
    db.upsert_file_state(project.id, &path_str, &hash, true)?;

    if verbose {
        eprintln!(
            "  {} {}",
            event_type,
            crate::relative_path(&path_str, &project.root_path)
        );
    }

    Ok(())
}

fn handle_modify(
    db: &Database,
    project: &WatchedProject,
    path: &Path,
    verbose: bool,
) -> Result<()> {
    if is_symlink(path) {
        return Ok(());
    }
    let content = match read_if_within_limit(path) {
        Some(c) => c,
        None => return Ok(()),
    };
    let hash = compute_hash(&content);
    let path_str = path.to_string_lossy().to_string();

    let state = db.get_file_state(project.id, &path_str)?;

    match &state {
        Some(s) if s.exists_now => {
            if s.latest_hash.as_deref() == Some(&hash) {
                return Ok(());
            }

            let snap = Some(snapshots::save(project.id, &hash, &content)?);

            db.insert_event(
                project.id,
                &path_str,
                "MODIFIED",
                Some(&hash),
                s.latest_hash.as_deref(),
                snap.as_deref(),
                None,
                Some(content.len() as i64),
            )?;
            db.upsert_file_state(project.id, &path_str, &hash, true)?;

            if verbose {
                eprintln!(
                    "  MODIFIED {}",
                    crate::relative_path(&path_str, &project.root_path)
                );
            }
        }
        _ => {
            return handle_create(db, project, path, verbose);
        }
    }

    Ok(())
}

fn handle_delete(
    db: &Database,
    project: &WatchedProject,
    path: &Path,
    verbose: bool,
) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();

    let prev_hash = db
        .get_file_state(project.id, &path_str)?
        .and_then(|s| if s.exists_now { s.latest_hash } else { None });

    if prev_hash.is_none() {
        return Ok(());
    }

    db.insert_event(
        project.id,
        &path_str,
        "DELETED",
        None,
        prev_hash.as_deref(),
        None,
        None,
        None,
    )?;
    db.mark_deleted(project.id, &path_str)?;

    if verbose {
        eprintln!(
            "  DELETED {}",
            crate::relative_path(&path_str, &project.root_path)
        );
    }

    Ok(())
}

fn handle_rename(
    db: &Database,
    project: &WatchedProject,
    old_path: &Path,
    new_path: &Path,
    verbose: bool,
) -> Result<()> {
    if is_symlink(new_path) {
        return Ok(());
    }
    let old_str = old_path.to_string_lossy().to_string();
    let new_str = new_path.to_string_lossy().to_string();

    let content = match read_if_within_limit(new_path) {
        Some(c) => c,
        None => return Ok(()),
    };
    let hash = compute_hash(&content);

    let prev_hash = db
        .get_file_state(project.id, &old_str)?
        .and_then(|s| if s.exists_now { s.latest_hash } else { None });

    let snap = Some(snapshots::save(project.id, &hash, &content)?);

    db.insert_event(
        project.id,
        &new_str,
        "RENAMED",
        Some(&hash),
        prev_hash.as_deref(),
        snap.as_deref(),
        Some(&old_str),
        Some(content.len() as i64),
    )?;

    db.mark_deleted(project.id, &old_str)?;
    db.upsert_file_state(project.id, &new_str, &hash, true)?;

    if verbose {
        eprintln!(
            "  RENAMED {} -> {}",
            crate::relative_path(&old_str, &project.root_path),
            crate::relative_path(&new_str, &project.root_path),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn initial_scan_rejects_directory_over_file_limit() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("file_{}.txt", i)), "data").unwrap();
        }

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        let err = initial_scan_with_limit(&db, &project, dir.path(), false, 5);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("too large to watch"), "got: {}", msg);
    }

    #[test]
    fn initial_scan_accepts_directory_under_file_limit() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("file_{}.txt", i)), "data").unwrap();
        }

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        let result = initial_scan_with_limit(&db, &project, dir.path(), false, 100);
        assert!(result.is_ok());
    }

    #[test]
    fn initial_scan_force_bypasses_file_limit() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("file_{}.txt", i)), "data").unwrap();
        }

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        let result = initial_scan(&db, &project, dir.path(), false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn root_accessible_returns_true_for_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(root_is_accessible(dir.path()));
    }

    #[test]
    fn root_accessible_returns_false_for_missing_dir() {
        assert!(!root_is_accessible(Path::new("/nonexistent/path/that/does/not/exist")));
    }

    #[test]
    fn root_accessible_returns_false_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not_a_dir");
        std::fs::write(&file, "data").unwrap();
        assert!(!root_is_accessible(&file));
    }
}
