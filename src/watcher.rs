use anyhow::Result;
use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use crate::db::Database;
use crate::ignore::should_ignore;
use crate::models::{FileState, WatchedProject};
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
    crate::to_hex(&result)
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
        if let Some(last) = self.last_event.get(path)
            && now.duration_since(*last) < Duration::from_millis(DEBOUNCE_MS)
        {
            return false;
        }
        self.last_event.insert(path.to_path_buf(), now);
        true
    }

    /// Periodically evict entries that are too old to matter for debouncing.
    fn maybe_cleanup(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_cleanup) < Duration::from_secs(DEBOUNCE_CLEANUP_SECS) {
            return;
        }
        self.last_event
            .retain(|_, t| now.duration_since(*t) < DEBOUNCE_MAX_AGE);
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

/// A single file's contribution to the scan, computed off the DB thread (read +
/// hash + snapshot write happen in parallel workers) and applied to the database
/// serially inside one transaction.
enum ScanWrite {
    /// File on disk matches its recorded hash — only refresh `exists_now`.
    Unchanged { path: String, hash: String },
    /// File not previously tracked.
    Created {
        path: String,
        hash: String,
        snapshot: String,
        size: i64,
    },
    /// File content changed since the last recorded state.
    Modified {
        path: String,
        hash: String,
        prev_hash: Option<String>,
        snapshot: String,
        size: i64,
    },
}

/// Worker pool size for the parallel scan. Capped because the work is a mix of
/// disk I/O and CPU (sha256 + gzip); a handful of threads saturates both without
/// oversubscribing small machines.
fn scan_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
}

/// Read, hash and (if changed) snapshot a single file. Pure with respect to the
/// database: it only *decides* what should be written, so it is safe to run on a
/// worker thread. Returns `Ok(None)` for files that are too large or unreadable.
fn scan_one(
    path_str: &str,
    existing: &HashMap<&str, &FileState>,
    project_id: i64,
    base: &Path,
) -> Result<Option<ScanWrite>> {
    let path = Path::new(path_str);
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if meta.len() > snapshots::MAX_SNAPSHOT_SIZE as u64 {
        return Ok(None);
    }
    let content = match std::fs::read(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let hash = compute_hash(&content);
    let size = content.len() as i64;

    match existing.get(path_str) {
        Some(state) if state.latest_hash.as_deref() == Some(hash.as_str()) => {
            Ok(Some(ScanWrite::Unchanged {
                path: path_str.to_string(),
                hash,
            }))
        }
        Some(state) => {
            let snapshot = snapshots::save_in(base, project_id, &hash, &content)?;
            Ok(Some(ScanWrite::Modified {
                path: path_str.to_string(),
                prev_hash: state.latest_hash.clone(),
                hash,
                snapshot,
                size,
            }))
        }
        None => {
            let snapshot = snapshots::save_in(base, project_id, &hash, &content)?;
            Ok(Some(ScanWrite::Created {
                path: path_str.to_string(),
                hash,
                snapshot,
                size,
            }))
        }
    }
}

/// Run the read+hash+snapshot pipeline across `paths`. Small scans run inline to
/// avoid pool setup; larger ones fan out to a bounded pool of scoped workers that
/// pull from a shared index. A read that hangs only parks the worker that issued
/// it — the pool is fixed-size, so a stuck filesystem can never leak unbounded
/// threads (the failure mode of the old thread-per-read helper).
fn report_scan_failure(path: &str, e: &anyhow::Error) {
    crate::log_warn!("scan: failed to snapshot {}: {}", path, e);
}

fn scan_pipeline(
    paths: &[String],
    existing: &HashMap<&str, &FileState>,
    project_id: i64,
    base: &Path,
) -> Vec<ScanWrite> {
    let workers = scan_worker_count();
    if workers <= 1 || paths.len() < 64 {
        return paths
            .iter()
            .filter_map(|p| match scan_one(p, existing, project_id, base) {
                Ok(write) => write,
                Err(e) => {
                    report_scan_failure(p, &e);
                    None
                }
            })
            .collect();
    }

    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<ScanWrite>();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(i) else { break };
                    match scan_one(path, existing, project_id, base) {
                        Ok(Some(write)) => {
                            let _ = tx.send(write);
                        }
                        Ok(None) => {}
                        Err(e) => report_scan_failure(path, &e),
                    }
                }
            });
        }
        // Drop the original sender so the channel closes once all workers finish.
        drop(tx);
        rx.iter().collect()
    })
}

fn initial_scan_with_limit(
    db: &Database,
    project: &WatchedProject,
    root: &Path,
    verbose: bool,
    max_files: usize,
) -> Result<()> {
    let existing_states = db.get_all_file_states(project.id)?;
    let existing: HashMap<&str, &FileState> = existing_states
        .iter()
        .map(|s| (s.path.as_str(), s))
        .collect();

    // Walk first, collecting candidate paths and enforcing the file-count cap
    // before doing any expensive read/hash work.
    let mut paths: Vec<String> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();
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
        let path_str = entry.path().to_string_lossy().to_string();
        seen_paths.insert(path_str.clone());
        paths.push(path_str);
    }

    // Read + hash + snapshot in parallel; the database is untouched until now.
    let base = crate::backtrack_dir()?;
    let writes = scan_pipeline(&paths, &existing, project.id, &base);
    let change_count = writes
        .iter()
        .filter(|w| !matches!(w, ScanWrite::Unchanged { .. }))
        .count();

    // Apply every change in a single transaction so the on-disk DB pays one
    // commit instead of one per file.
    let deletions = db.transaction(|db| -> Result<usize> {
        for write in &writes {
            match write {
                ScanWrite::Unchanged { path, hash } => {
                    db.upsert_file_state(project.id, path, hash, true)?;
                }
                ScanWrite::Created {
                    path,
                    hash,
                    snapshot,
                    size,
                } => {
                    db.insert_event(
                        project.id,
                        path,
                        "CREATED",
                        Some(hash),
                        None,
                        Some(snapshot),
                        None,
                        Some(*size),
                    )?;
                    db.upsert_file_state(project.id, path, hash, true)?;
                }
                ScanWrite::Modified {
                    path,
                    hash,
                    prev_hash,
                    snapshot,
                    size,
                } => {
                    db.insert_event(
                        project.id,
                        path,
                        "MODIFIED",
                        Some(hash),
                        prev_hash.as_deref(),
                        Some(snapshot),
                        None,
                        Some(*size),
                    )?;
                    db.upsert_file_state(project.id, path, hash, true)?;
                    if verbose {
                        eprintln!(
                            "  scan: MODIFIED {}",
                            crate::relative_path(path, &project.root_path)
                        );
                    }
                }
            }
        }

        // Detect deletions that happened while the daemon was stopped.
        let mut deletions = 0usize;
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
                deletions += 1;
                if verbose {
                    eprintln!(
                        "  scan: DELETED {}",
                        crate::relative_path(&state.path, &project.root_path)
                    );
                }
            }
        }
        Ok(deletions)
    })?;

    let count = change_count + deletions;
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
                crate::log_warn!("watched directory is no longer accessible — pausing recording");
                paused = true;
            } else if accessible && paused {
                crate::log_notice!("watched directory is accessible again — resuming");
                if let Err(e) = initial_scan(db, project, root, verbose, true) {
                    crate::log_warn!("reconciliation scan failed: {}", e);
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
                    crate::log_notice!(
                        "auto-prune: removed {} events, {} snapshots, {} backups (freed {})",
                        stats.events_deleted,
                        stats.snapshots_deleted,
                        stats.backups_deleted,
                        crate::retention::format_size(stats.bytes_freed),
                    );
                }
                Err(e) => crate::log_warn!("auto-prune failed: {}", e),
                _ => {}
            }
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                if paused {
                    continue;
                }
                // Guard each event so a single panic (a bug in one handler, a
                // malformed event) can never unwind and kill the whole daemon —
                // which would stop recording silently while still appearing to
                // run. Log and move on to the next event instead.
                match guard_event(|| {
                    process_event(db, project, root, event, &mut debouncer, verbose)
                }) {
                    EventOutcome::Ok => {}
                    EventOutcome::Failed(e) => {
                        // Always surface errors — a silent failure means the user
                        // believes changes are being recorded when they aren't.
                        crate::log_warn!("failed to record event: {}", e);
                    }
                    EventOutcome::Panicked => {
                        crate::log_warn!(
                            "panicked while processing an event — skipping it; daemon continues"
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                // Always surface watcher errors too.
                crate::log_warn!("file watcher error: {}", e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

// ── event dispatch ──────────────────────────────────────────────────

/// Result of attempting to process one watcher event, including the panic case
/// so the watch loop can keep running after any single failure.
enum EventOutcome {
    Ok,
    Failed(String),
    Panicked,
}

/// Run `f` (the per-event processing) catching both errors and panics.
/// `AssertUnwindSafe` is required because the closure mutably borrows the
/// debouncer and holds a DB handle; that is sound here because on a panic we
/// discard the event and continue — no partially-mutated state is observed
/// across the boundary in a way that could violate invariants (DB writes are
/// autocommit; the debouncer is just a timing cache).
fn guard_event<F: FnOnce() -> Result<()>>(f: F) -> EventOutcome {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => EventOutcome::Ok,
        Ok(Err(e)) => EventOutcome::Failed(e.to_string()),
        Err(_) => EventOutcome::Panicked,
    }
}

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

    if let Some(ref s) = state
        && s.latest_hash.as_deref() == Some(&hash)
        && s.exists_now
    {
        return Ok(());
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

    /// guard_event maps a successful handler to Ok.
    #[test]
    fn guard_event_passes_through_success() {
        assert!(matches!(guard_event(|| Ok(())), EventOutcome::Ok));
    }

    /// guard_event surfaces a handler error as Failed without unwinding.
    #[test]
    fn guard_event_reports_errors() {
        let outcome = guard_event(|| anyhow::bail!("boom"));
        match outcome {
            EventOutcome::Failed(msg) => assert!(msg.contains("boom")),
            _ => panic!("expected Failed"),
        }
    }

    /// The crux of the fix: a panicking handler is caught and reported as
    /// Panicked rather than unwinding the watch loop and killing the daemon.
    /// (Before the guard, this panic propagated out of `process_event` and
    /// took the whole daemon down.)
    #[test]
    fn guard_event_catches_panics() {
        // Silence the default panic hook so the caught panic doesn't spam the
        // test output with a backtrace.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = guard_event(|| panic!("simulated handler panic"));
        std::panic::set_hook(prev);

        assert!(
            matches!(outcome, EventOutcome::Panicked),
            "a panic in event processing must be caught, not propagated"
        );
    }

    /// A directory exceeding the file limit is rejected with a clear error unless --force is set.
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

    /// Throwaway benchmark (ignored by default) for issue #22: how long does a
    /// cold `initial_scan` take on a realistic tree? Run in release for real numbers:
    ///   BENCH_FILES=20000 BENCH_FILE_BYTES=4096 \
    ///     cargo test --release -p undo bench_initial_scan -- --ignored --nocapture
    #[test]
    #[ignore = "perf benchmark; run explicitly with --ignored --release"]
    fn bench_initial_scan() {
        let n: usize = std::env::var("BENCH_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000);
        let bytes: usize = std::env::var("BENCH_FILE_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4096);
        let dirs = 100usize;

        let on_disk = std::env::var("BENCH_DISK_DB").is_ok();
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();

        // Build the tree: N distinct files spread across `dirs` subdirectories,
        // each with unique content so every file is hashed, snapshotted and inserted.
        let setup = Instant::now();
        for d in 0..dirs {
            let sub = tree.path().join(format!("dir_{d:03}"));
            std::fs::create_dir_all(&sub).unwrap();
        }
        for i in 0..n {
            let sub = tree.path().join(format!("dir_{:03}", i % dirs));
            let mut content = format!("// file {i}\n").into_bytes();
            content.resize(bytes, (i % 251) as u8);
            std::fs::write(sub.join(format!("f_{i:06}.rs")), &content).unwrap();
        }
        let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;

        let db = if on_disk {
            Database::open().unwrap()
        } else {
            Database::open_in_memory().unwrap()
        };
        let project = db.get_or_create_project(tree.path()).unwrap();

        let started = Instant::now();
        initial_scan_with_limit(&db, &project, tree.path(), false, usize::MAX).unwrap();
        let scan = started.elapsed();
        let scan_ms = scan.as_secs_f64() * 1000.0;

        let total_bytes = (n * bytes) as f64;
        let mb_per_s = (total_bytes / (1024.0 * 1024.0)) / scan.as_secs_f64();
        eprintln!(
            "\n[bench_initial_scan] files={n} size={bytes}B db={}  setup={setup_ms:.0}ms  \
             scan={scan_ms:.1}ms  ({:.1} files/s, {mb_per_s:.0} MB/s)\n",
            if on_disk { "disk" } else { "memory" },
            n as f64 / scan.as_secs_f64(),
        );
    }

    /// Decomposition benchmark (ignored) for issue #22: attribute the scan cost to
    /// (1) read+hash with the thread-per-read helper, (2) read+hash with an inline
    /// read, and (3) the snapshot-write + DB-insert remainder. Run:
    ///   BENCH_FILES=20000 cargo test --release -p undo bench_scan_phases -- --ignored --nocapture
    #[test]
    #[ignore = "perf benchmark; run explicitly with --ignored --release"]
    fn bench_scan_phases() {
        let n: usize = std::env::var("BENCH_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000);
        let bytes: usize = std::env::var("BENCH_FILE_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4096);
        let dirs = 100usize;

        let tree = tempfile::tempdir().unwrap();
        for d in 0..dirs {
            std::fs::create_dir_all(tree.path().join(format!("dir_{d:03}"))).unwrap();
        }
        for i in 0..n {
            let sub = tree.path().join(format!("dir_{:03}", i % dirs));
            let mut content = format!("// file {i}\n").into_bytes();
            content.resize(bytes, (i % 251) as u8);
            std::fs::write(sub.join(format!("f_{i:06}.rs")), &content).unwrap();
        }

        let collect_paths = || {
            WalkDir::new(tree.path())
                .into_iter()
                .filter_entry(|e| !should_ignore(e.path(), tree.path()))
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && !e.path_is_symlink())
                .map(|e| e.path().to_path_buf())
                .collect::<Vec<_>>()
        };

        // Phase A: read+hash via the current thread-per-read helper.
        let paths = collect_paths();
        let t = Instant::now();
        let mut acc = 0u8;
        for p in &paths {
            if let Some(c) = read_if_within_limit(p) {
                acc ^= compute_hash(&c).as_bytes()[0];
            }
        }
        let with_thread = t.elapsed().as_secs_f64() * 1000.0;

        // Phase B: read+hash via a plain inline read (no spawn, no channel).
        let t = Instant::now();
        for p in &paths {
            if let Ok(c) = std::fs::read(p) {
                acc ^= compute_hash(&c).as_bytes()[0];
            }
        }
        let inline = t.elapsed().as_secs_f64() * 1000.0;

        eprintln!(
            "\n[bench_scan_phases] files={n} size={bytes}B  \
             read+hash(thread-per-read)={with_thread:.1}ms  \
             read+hash(inline)={inline:.1}ms  \
             thread-per-read overhead={:.1}ms ({:.0}us/file)  [acc={acc}]\n",
            with_thread - inline,
            (with_thread - inline) * 1000.0 / n as f64,
        );
    }

    /// A directory within the file limit is scanned without error.
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

    /// --force overrides the file count limit so large repos can be watched explicitly.
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

    /// A scan with more files than the parallel threshold exercises the worker
    /// pool (not the serial fallback): every file must be recorded as CREATED
    /// with a backing snapshot.
    #[test]
    fn initial_scan_parallel_records_all_files() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let n = 250usize; // > 64 → parallel path
        for i in 0..n {
            std::fs::write(
                dir.path().join(format!("file_{i:04}.txt")),
                format!("unique content {i}"),
            )
            .unwrap();
        }

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX).unwrap();

        let states = db.get_all_file_states(project.id).unwrap();
        assert_eq!(states.len(), n, "every file must have a recorded state");
        assert!(states.iter().all(|s| s.exists_now));
        // Distinct content → one snapshot per file.
        assert_eq!(snapshots::count(project.id).unwrap(), n);
        for i in [0usize, n / 2, n - 1] {
            let p = dir
                .path()
                .join(format!("file_{i:04}.txt"))
                .to_string_lossy()
                .to_string();
            let ev = db.get_latest_event(project.id, &p).unwrap().unwrap();
            assert_eq!(ev.event_type, "CREATED");
        }
    }

    /// Many files sharing identical content hit the same snapshot hash from
    /// multiple workers at once. Snapshot writes must dedup to one file per
    /// distinct hash without racing on a shared temp path, and every source
    /// file must still be tracked.
    #[test]
    fn initial_scan_parallel_dedups_identical_content() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let distinct = 5usize;
        let copies = 40usize; // distinct * copies = 200 files > 64 → parallel
        for d in 0..distinct {
            for c in 0..copies {
                std::fs::write(
                    dir.path().join(format!("g{d}_c{c:03}.txt")),
                    format!("shared body {d}"),
                )
                .unwrap();
            }
        }

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX).unwrap();

        let states = db.get_all_file_states(project.id).unwrap();
        assert_eq!(states.len(), distinct * copies, "all files tracked");
        assert_eq!(
            snapshots::count(project.id).unwrap(),
            distinct,
            "identical content must produce one snapshot per distinct hash"
        );
    }

    /// An existing directory is reported as accessible.
    #[test]
    fn root_accessible_returns_true_for_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(root_is_accessible(dir.path()));
    }

    /// A non-existent path is not accessible.
    #[test]
    fn root_accessible_returns_false_for_missing_dir() {
        assert!(!root_is_accessible(Path::new(
            "/nonexistent/path/that/does/not/exist"
        )));
    }

    /// A file path is not a directory and must not be considered accessible.
    #[test]
    fn root_accessible_returns_false_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not_a_dir");
        std::fs::write(&file, "data").unwrap();
        assert!(!root_is_accessible(&file));
    }

    /// The same bytes always produce the same 64-char hex hash; different bytes produce a different hash.
    #[test]
    fn compute_hash_is_stable_and_input_sensitive() {
        let h1 = compute_hash(b"hello world");
        let h2 = compute_hash(b"hello world");
        let h3 = compute_hash(b"hello WORLD");
        assert_eq!(h1, h2, "same input must produce same hash");
        assert_ne!(h1, h3, "different input must produce different hash");
        // SHA-256 hex output is always 64 lowercase hex chars.
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A file tracked in the DB but absent from disk triggers a DELETED event and marks exists_now false.
    #[test]
    fn initial_scan_records_deletion_for_missing_file() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        // Seed the DB with a file that no longer exists on disk.
        let phantom_path = dir.path().join("phantom.rs").to_string_lossy().to_string();
        db.upsert_file_state(project.id, &phantom_path, "deadbeef", true)
            .unwrap();

        // Run the scan on the empty directory — phantom.rs is missing.
        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX).unwrap();

        // The file's state must now be marked deleted.
        let state = db
            .get_file_state(project.id, &phantom_path)
            .unwrap()
            .unwrap();
        assert!(!state.exists_now);

        // A DELETED event must have been recorded.
        let event = db
            .get_latest_event(project.id, &phantom_path)
            .unwrap()
            .unwrap();
        assert_eq!(event.event_type, "DELETED");
    }
}
