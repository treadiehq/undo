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

// ── hashing ─────────────────────────────────────────────────────────

fn compute_hash(data: &[u8]) -> String {
    let result = Sha256::digest(data);
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

// ── debouncer ───────────────────────────────────────────────────────

struct Debouncer {
    last_event: HashMap<PathBuf, Instant>,
}

impl Debouncer {
    fn new() -> Self {
        Self {
            last_event: HashMap::new(),
        }
    }

    fn should_process(&mut self, path: &Path) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_event.get(path) {
            if now.duration_since(*last) < Duration::from_millis(DEBOUNCE_MS) {
                return false;
            }
        }
        self.last_event.insert(path.to_path_buf(), now);
        true
    }
}

// ── initial scan ────────────────────────────────────────────────────

pub fn initial_scan(
    db: &Database,
    project: &WatchedProject,
    root: &Path,
    verbose: bool,
) -> Result<()> {
    let existing_states = db.get_all_file_states(project.id)?;
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut count = 0usize;

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !should_ignore(e.path(), root))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();
        seen_paths.insert(path_str.clone());

        let content = match std::fs::read(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let hash = compute_hash(&content);
        let existing = db.get_file_state(project.id, &path_str)?;

        match existing {
            Some(ref state) if state.latest_hash.as_deref() == Some(hash.as_str()) => {
                db.upsert_file_state(project.id, &path_str, &hash, true)?;
            }
            Some(ref state) => {
                let prev_hash = state.latest_hash.as_deref();
                let snap = if content.len() <= snapshots::MAX_SNAPSHOT_SIZE {
                    Some(snapshots::save(project.id, &hash, &content)?)
                } else {
                    None
                };
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
                let snap = if content.len() <= snapshots::MAX_SNAPSHOT_SIZE {
                    Some(snapshots::save(project.id, &hash, &content)?)
                } else {
                    None
                };
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

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                if let Err(e) =
                    process_event(db, project, root, event, &mut debouncer, verbose)
                {
                    if verbose {
                        eprintln!("Error processing event: {}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                if verbose {
                    eprintln!("Watch error: {}", e);
                }
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
                if !should_ignore(new, root) && debouncer.should_process(new) {
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

fn handle_create(
    db: &Database,
    project: &WatchedProject,
    path: &Path,
    verbose: bool,
) -> Result<()> {
    let content = std::fs::read(path)?;
    let hash = compute_hash(&content);
    let path_str = path.to_string_lossy().to_string();

    let state = db.get_file_state(project.id, &path_str)?;

    if let Some(ref s) = state {
        if s.latest_hash.as_deref() == Some(&hash) && s.exists_now {
            return Ok(());
        }
    }

    let snap = if content.len() <= snapshots::MAX_SNAPSHOT_SIZE {
        Some(snapshots::save(project.id, &hash, &content)?)
    } else {
        None
    };

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
    let content = match std::fs::read(path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let hash = compute_hash(&content);
    let path_str = path.to_string_lossy().to_string();

    let state = db.get_file_state(project.id, &path_str)?;

    match &state {
        Some(s) if s.exists_now => {
            if s.latest_hash.as_deref() == Some(&hash) {
                return Ok(());
            }

            let snap = if content.len() <= snapshots::MAX_SNAPSHOT_SIZE {
                Some(snapshots::save(project.id, &hash, &content)?)
            } else {
                None
            };

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
    let old_str = old_path.to_string_lossy().to_string();
    let new_str = new_path.to_string_lossy().to_string();

    let content = match std::fs::read(new_path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let hash = compute_hash(&content);

    let prev_hash = db
        .get_file_state(project.id, &old_str)?
        .and_then(|s| s.latest_hash);

    let snap = if content.len() <= snapshots::MAX_SNAPSHOT_SIZE {
        Some(snapshots::save(project.id, &hash, &content)?)
    } else {
        None
    };

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
