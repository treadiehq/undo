use anyhow::Result;
use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use crate::db::Database;
use crate::ignore::should_ignore_with_type;
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

/// A unit of work for the shared filesystem watchdog. The closure runs the op
/// *and* delivers its own result, so the watchdog only has to execute it.
type FsJob = Box<dyn FnOnce() + Send + 'static>;

/// Submission handle for the process-wide watchdog thread, started on first use.
///
/// A single long-lived worker — rather than spawning a thread per op — bounds the
/// cost of a wedged filesystem (#29). If an op hangs in a syscall this one thread
/// parks on it and ops queued behind it time out, but no *new* threads are ever
/// spawned, so a sustained outage costs exactly one parked thread instead of
/// leaking them without bound. Every caller runs on the single watch-loop thread,
/// so ops are already submitted serially; the worker has no concurrent work to
/// overlap and a single thread is sufficient.
fn fs_watchdog() -> &'static Mutex<mpsc::Sender<FsJob>> {
    static WATCHDOG: OnceLock<Mutex<mpsc::Sender<FsJob>>> = OnceLock::new();
    WATCHDOG.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<FsJob>();
        std::thread::Builder::new()
            .name("undo-fs-watchdog".to_string())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // A panicking op must not take down the shared watchdog —
                    // that would disable fs timeouts process-wide. Catch it and
                    // keep serving; the caller sees None because its result
                    // channel is dropped during unwind, exactly as on a timeout.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }
            })
            .expect("spawn fs watchdog thread");
        Mutex::new(tx)
    })
}

/// Run a filesystem operation on the shared watchdog thread with a timeout.
/// Returns None if it doesn't finish within `FS_TIMEOUT` — because it hung, or
/// because a prior op is still hanging ahead of it in the queue. Either way the
/// caller skips the op rather than letting it wedge the watch loop.
fn fs_with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    let job: FsJob = Box::new(move || {
        let _ = tx.send(f());
    });
    if fs_watchdog().lock().ok()?.send(job).is_err() {
        return None;
    }
    rx.recv_timeout(FS_TIMEOUT).ok()
}

/// `symlink_metadata` under the fs watchdog (#28). A hung `stat()` on a flaky
/// network mount would otherwise block `process_event` indefinitely — and since
/// the watch loop runs event processing inline, that also starves the health
/// check meant to detect the dead mount and pause recording. Returns None on a
/// stat error *or* timeout; callers treat both the same as "skip this event",
/// after which the (also timeout-guarded) health check pauses recording.
fn symlink_metadata_timeout(path: &Path) -> Option<std::fs::Metadata> {
    let p = path.to_path_buf();
    fs_with_timeout(move || std::fs::symlink_metadata(&p).ok())?
}

// ── hashing ─────────────────────────────────────────────────────────

fn compute_hash(data: &[u8]) -> String {
    let result = Sha256::digest(data);
    crate::to_hex(&result)
}

/// Modification time as nanoseconds since the Unix epoch, or `None` if the
/// filesystem doesn't report a usable mtime. Paired with the file size, this
/// lets modify events short-circuit the read+hash when nothing changed (#26).
fn mtime_nanos(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
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
    // Startup may legitimately observe files deleted while the daemon was off, so
    // deletions are trusted here — the empty-tree guard inside still protects
    // against starting against an empty/wrong mount (#31).
    initial_scan_with_limit(db, project, root, verbose, max_files, true)
}

/// Reconcile after the watched root became accessible again following a pause.
///
/// `start_dev` is the root's device id captured when watching began. If it no
/// longer matches, the root is a *different* filesystem object (swapped disk or
/// fresh remount) and the now-missing files must not be recorded as deletions —
/// doing so would let prune reclaim their snapshots and lose history for what is
/// really a transient mount event (#31). We still reconcile any files actually
/// present so recording resumes; we just don't trust the disappearances.
fn reconcile_after_resume(
    db: &Database,
    project: &WatchedProject,
    root: &Path,
    start_dev: Option<u64>,
    verbose: bool,
) -> Result<()> {
    let same_device = match (start_dev, root_device_id(root)) {
        (Some(a), Some(b)) => a == b,
        // Unknown device on either side (stat failed/timed out): be conservative
        // and don't record deletions.
        _ => false,
    };
    // Resume was already accepted at startup, so don't re-trip the file-count cap.
    initial_scan_with_limit(db, project, root, verbose, usize::MAX, same_device)
}

/// Device id (`st_dev`) of the watched root, taken through the fs watchdog so it
/// can't hang on a wedged mount (#28/#29). Used to tell whether the root is still
/// the same filesystem object across a pause/resume (#31).
fn root_device_id(root: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let r = root.to_path_buf();
    fs_with_timeout(move || std::fs::metadata(&r).ok().map(|m| m.dev()))?
}

/// A single file's contribution to the scan, computed off the DB thread (read +
/// hash + snapshot write happen in parallel workers) and applied to the database
/// serially inside one transaction.
enum ScanWrite {
    /// File on disk matches its recorded hash — refresh `exists_now` and the
    /// size/mtime so the live fast path (#26) can short-circuit later events.
    Unchanged {
        path: String,
        hash: String,
        size: i64,
        mtime_nanos: Option<i64>,
    },
    /// File not previously tracked.
    Created {
        path: String,
        hash: String,
        snapshot: String,
        size: i64,
        mtime_nanos: Option<i64>,
    },
    /// File content changed since the last recorded state.
    Modified {
        path: String,
        hash: String,
        prev_hash: Option<String>,
        snapshot: String,
        size: i64,
        mtime_nanos: Option<i64>,
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
    publish_guard: &snapshots::PublishGuard,
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
    let mtime = mtime_nanos(&meta);
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
                size,
                mtime_nanos: mtime,
            }))
        }
        Some(state) => {
            let snapshot = snapshots::save_in(publish_guard, base, project_id, &hash, &content)?;
            Ok(Some(ScanWrite::Modified {
                path: path_str.to_string(),
                prev_hash: state.latest_hash.clone(),
                hash,
                snapshot,
                size,
                mtime_nanos: mtime,
            }))
        }
        None => {
            let snapshot = snapshots::save_in(publish_guard, base, project_id, &hash, &content)?;
            Ok(Some(ScanWrite::Created {
                path: path_str.to_string(),
                hash,
                snapshot,
                size,
                mtime_nanos: mtime,
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
    crate::log_warn!(
        "File skipped: could not save a version of {} during the initial scan: {}",
        path,
        e
    );
}

fn scan_pipeline(
    paths: &[String],
    existing: &HashMap<&str, &FileState>,
    publish_guard: &snapshots::PublishGuard,
    project_id: i64,
    base: &Path,
) -> Vec<ScanWrite> {
    let workers = scan_worker_count();
    if workers <= 1 || paths.len() < 64 {
        return paths
            .iter()
            .filter_map(
                |p| match scan_one(p, existing, publish_guard, project_id, base) {
                    Ok(write) => write,
                    Err(e) => {
                        report_scan_failure(p, &e);
                        None
                    }
                },
            )
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
                    match scan_one(path, existing, publish_guard, project_id, base) {
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
    trust_deletions: bool,
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
        .filter_entry(|e| !should_ignore_with_type(e.path(), root, e.file_type().is_dir()))
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
                "Recording did not start: this directory is too large to watch safely (more than {} files).\n\
                 Rerun with --force to override this limit.",
                max_files
            );
        }
        let Some(path_str) = db_key(entry.path()) else {
            continue;
        };
        seen_paths.insert(path_str.clone());
        paths.push(path_str);
    }

    // Read + hash + snapshot in parallel; the database is untouched until now.
    let base = crate::backtrack_dir()?;
    let publish_guard = snapshots::acquire_publish_guard()?;
    let writes = scan_pipeline(&paths, &existing, &publish_guard, project.id, &base);
    let change_count = writes
        .iter()
        .filter(|w| !matches!(w, ScanWrite::Unchanged { .. }))
        .count();

    // Apply every change in a single transaction so the on-disk DB pays one
    // commit instead of one per file.
    let deletions = db.transaction(|db| -> Result<usize> {
        for write in &writes {
            match write {
                ScanWrite::Unchanged {
                    path,
                    hash,
                    size,
                    mtime_nanos,
                } => {
                    db.upsert_file_state(project.id, path, hash, true, *size, *mtime_nanos)?;
                }
                ScanWrite::Created {
                    path,
                    hash,
                    snapshot,
                    size,
                    mtime_nanos,
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
                    db.upsert_file_state(project.id, path, hash, true, *size, *mtime_nanos)?;
                }
                ScanWrite::Modified {
                    path,
                    hash,
                    prev_hash,
                    snapshot,
                    size,
                    mtime_nanos,
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
                    db.upsert_file_state(project.id, path, hash, true, *size, *mtime_nanos)?;
                    if verbose {
                        eprintln!(
                            "  scan: MODIFIED {}",
                            crate::relative_path(path, &project.root_path)
                        );
                    }
                }
            }
        }

        // Detect deletions that happened while the daemon was stopped — but guard
        // against a mount anomaly masquerading as mass deletion (#31). If the tree
        // came back empty, or the root is a different filesystem object than when
        // watching started (`trust_deletions == false`), treat the vanished files
        // as a transient mount event, not real deletions: recording them would
        // flip `exists_now` to 0, and `get_live_hashes` only pins a snapshot via
        // `file_state` while `exists_now = 1`, so prune would then reclaim the
        // snapshots — turning a remount into permanent history loss. Leaving the
        // files tracked preserves history; the worst case if they really were
        // deleted is slightly stale "exists" state until the next scan.
        let would_delete = existing_states
            .iter()
            .filter(|s| s.exists_now && !seen_paths.contains(&s.path))
            .count();
        let mount_anomaly = would_delete > 0 && (seen_paths.is_empty() || !trust_deletions);

        let mut deletions = 0usize;
        if mount_anomaly {
            crate::log_warn!(
                "Deletions skipped: Undo found {} missing paths, but the watched directory {}. \
                 Existing saved versions were kept; no deletions were recorded (#31)",
                would_delete,
                if seen_paths.is_empty() {
                    "came back empty (likely an empty remount)"
                } else {
                    "is a different filesystem than at startup (likely a swapped mount)"
                }
            );
        } else {
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
        }
        Ok(deletions)
    })?;

    let count = change_count + deletions;
    if count > 0 {
        eprintln!(
            "Initial scan: detected {} file change{}.",
            count,
            if count == 1 { "" } else { "s" }
        );
    }

    Ok(())
}

// ── live watcher ────────────────────────────────────────────────────

/// How often to verify the watched root directory is still accessible.
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How often to auto-prune old history.
const AUTO_PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

fn root_is_accessible(root: &Path) -> bool {
    // Guarded so the periodic health check itself can't hang on the very dead
    // mount it exists to detect (#28). A timed-out probe counts as inaccessible,
    // which pauses recording — exactly the intended response to a wedged mount.
    let r = root.to_path_buf();
    fs_with_timeout(move || r.try_exists().unwrap_or(false) && r.is_dir()).unwrap_or(false)
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

    // Identity of the watched filesystem when recording began. If the root comes
    // back as a different device after a pause, the resume scan must not treat the
    // vanished files as deletions (#31).
    let start_dev = root_device_id(root);

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
                crate::log_warn!(
                    "Recording paused: the watched directory is no longer accessible: {}",
                    root.display()
                );
                paused = true;
            } else if accessible && paused {
                crate::log_notice!(
                    "Recording resumed: the watched directory is accessible again: {}",
                    root.display()
                );
                if let Err(e) = reconcile_after_resume(db, project, root, start_dev, verbose) {
                    crate::log_warn!("Recording resumed, but the catch-up scan failed: {}", e);
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
                        "Automatic cleanup removed {} file changes, {} saved versions, and {} backups; freed {}.",
                        stats.events_deleted,
                        stats.snapshots_deleted,
                        stats.backups_deleted,
                        crate::retention::format_size(stats.bytes_freed),
                    );
                }
                Err(e) => crate::log_warn!("Automatic cleanup failed: {}", e),
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
                        crate::log_warn!("File skipped: Undo could not record this change: {}", e);
                    }
                    EventOutcome::Panicked => {
                        crate::log_warn!(
                            "File skipped: the change handler panicked; recording continues"
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                // Always surface watcher errors too.
                crate::log_warn!("Recording issue: the file watcher reported an error: {}", e);
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
                // One stat per path: its type drives the ignore check (is_dir),
                // the regular-file gate, and the snapshot size limit downstream.
                // Timeout-guarded so a hung mount can't wedge the loop (#28).
                let Some(meta) = symlink_metadata_timeout(path) else {
                    continue;
                };
                if should_ignore_with_type(path, root, meta.is_dir()) || !meta.is_file() {
                    continue;
                }
                if debouncer.should_process(path) {
                    handle_create(db, project, path, meta.len(), mtime_nanos(&meta), verbose)?;
                }
            }
        }

        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any) => {
            for path in &event.paths {
                let Some(meta) = symlink_metadata_timeout(path) else {
                    continue;
                };
                if should_ignore_with_type(path, root, meta.is_dir()) || !meta.is_file() {
                    continue;
                }
                if debouncer.should_process(path) {
                    handle_modify(db, project, path, meta.len(), mtime_nanos(&meta), verbose)?;
                }
            }
        }

        EventKind::Remove(_) => {
            if !root_is_accessible(root) {
                return Ok(());
            }
            for path in &event.paths {
                // The path is gone, so there is no type to stat; pass is_dir=false
                // (a deleted path resolves the same way) and let the matcher fall
                // back to its name/extension rules. Avoids an unguarded is_dir()
                // stat that could hang on a dead mount (#28).
                if should_ignore_with_type(path, root, false) {
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
                let new_meta = symlink_metadata_timeout(new);
                let new_is_dir = new_meta.as_ref().is_some_and(|m| m.is_dir());
                if should_ignore_with_type(new, root, new_is_dir) {
                    // `old` has been renamed away, so its type is moot: pass
                    // is_dir=false to avoid an unguarded stat (#28).
                    if !should_ignore_with_type(old, root, false) && debouncer.should_process(old) {
                        handle_delete(db, project, old, verbose)?;
                    }
                } else if debouncer.should_process(new) {
                    // Only regular files become RENAMED events; symlinks and
                    // directories are no-ops, matching the old is_symlink guard.
                    if let Some(meta) = new_meta.filter(|m| m.is_file()) {
                        handle_rename(
                            db,
                            project,
                            old,
                            new,
                            meta.len(),
                            mtime_nanos(&meta),
                            verbose,
                        )?;
                    }
                }
            } else {
                for path in &event.paths {
                    let meta = symlink_metadata_timeout(path);
                    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
                    if should_ignore_with_type(path, root, is_dir) {
                        continue;
                    }
                    match &meta {
                        // Symlinks are never recorded — skip without a delete.
                        Some(m) if m.file_type().is_symlink() => {}
                        Some(m) if m.is_file() => {
                            if debouncer.should_process(path) {
                                handle_modify(db, project, path, m.len(), mtime_nanos(m), verbose)?;
                            }
                        }
                        // Missing (renamed away) or a directory: a delete, which
                        // is a no-op for anything not tracked as a file.
                        _ => {
                            if debouncer.should_process(path) {
                                handle_delete(db, project, path, verbose)?;
                            }
                        }
                    }
                }
            }
        }

        _ => {}
    }

    Ok(())
}

// ── per-event handlers ──────────────────────────────────────────────

/// Read a file whose size (`len`, already known from the caller's stat) is within
/// `MAX_SNAPSHOT_SIZE`. The read itself still runs under `fs_with_timeout` so a
/// hung filesystem can't wedge the watch loop. Returns None for oversized files
/// or if the read times out.
fn read_within_limit(path: &Path, len: u64) -> Option<Vec<u8>> {
    if len > snapshots::MAX_SNAPSHOT_SIZE as u64 {
        return None;
    }
    let p = path.to_path_buf();
    fs_with_timeout(move || std::fs::read(&p).ok())?
}

/// The database key for a path: its UTF-8 string, or None if the path is not
/// valid UTF-8. Paths live in TEXT columns, and `to_string_lossy` would silently
/// remap non-UTF8 bytes to U+FFFD — a key that no longer matches the real file
/// and can even collide with another mangled name, so the file would be tracked,
/// diffed, or restored wrongly without any sign. Rather than store a corrupt key,
/// skip the file and surface a warning so the gap is visible (#34).
fn db_key(path: &Path) -> Option<String> {
    match path.to_str() {
        Some(s) => Some(s.to_string()),
        None => {
            crate::log_warn!(
                "File skipped: its path is not valid UTF-8, so Undo cannot track it safely: {}",
                path.display()
            );
            None
        }
    }
}

fn handle_create(
    db: &Database,
    project: &WatchedProject,
    path: &Path,
    len: u64,
    mtime_nanos: Option<i64>,
    verbose: bool,
) -> Result<()> {
    let Some(path_str) = db_key(path) else {
        return Ok(());
    };
    let content = match read_within_limit(path, len) {
        Some(c) => c,
        None => return Ok(()),
    };
    let hash = compute_hash(&content);

    let state = db.get_file_state(project.id, &path_str)?;

    if let Some(ref s) = state
        && s.latest_hash.as_deref() == Some(&hash)
        && s.exists_now
    {
        // Content unchanged; refresh stored size/mtime so the modify fast path
        // can short-circuit subsequent events without re-reading.
        db.upsert_file_state(
            project.id,
            &path_str,
            &hash,
            true,
            content.len() as i64,
            mtime_nanos,
        )?;
        return Ok(());
    }

    // Durable: a live-path snapshot may be the only copy of this content, so it
    // must survive power loss before the event that references it commits (#41).
    let publish_guard = snapshots::acquire_publish_guard()?;
    let snap = Some(snapshots::save_durable(
        &publish_guard,
        project.id,
        &hash,
        &content,
    )?);

    // macOS FSEvents can report overwrites as CREATE events.
    // If the file is already tracked and alive, record MODIFIED instead.
    let (event_type, prev_hash) = match &state {
        Some(s) if s.exists_now => ("MODIFIED", s.latest_hash.clone()),
        _ => ("CREATED", None),
    };

    // One transaction so the event and the file_state update are all-or-nothing:
    // a crash between them can't leave a dangling previous_hash chain or a stale
    // exists_now flag (#41).
    db.transaction(|db| {
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
        db.upsert_file_state(
            project.id,
            &path_str,
            &hash,
            true,
            content.len() as i64,
            mtime_nanos,
        )?;
        Ok(())
    })?;

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
    len: u64,
    mtime_nanos: Option<i64>,
    verbose: bool,
) -> Result<()> {
    let Some(path_str) = db_key(path) else {
        return Ok(());
    };
    let state = db.get_file_state(project.id, &path_str)?;

    // Fast path (#26): a recorded file whose size *and* mtime are unchanged has
    // unchanged bytes, so skip the (up to 100 MiB) read + hash entirely. Any
    // mismatch — or a missing/legacy mtime — falls through to the full read.
    if let Some(s) = &state
        && s.exists_now
        && s.latest_hash.is_some()
        && mtime_nanos.is_some()
        && s.size == Some(len as i64)
        && s.mtime_nanos == mtime_nanos
    {
        return Ok(());
    }

    let content = match read_within_limit(path, len) {
        Some(c) => c,
        None => return Ok(()),
    };
    let hash = compute_hash(&content);

    match &state {
        Some(s) if s.exists_now => {
            if s.latest_hash.as_deref() == Some(&hash) {
                // Bytes unchanged but size/mtime moved (e.g. a no-op rewrite that
                // bumped mtime): refresh the stored stat so the next event can
                // take the fast path, without recording a spurious MODIFIED.
                db.upsert_file_state(
                    project.id,
                    &path_str,
                    &hash,
                    true,
                    content.len() as i64,
                    mtime_nanos,
                )?;
                return Ok(());
            }

            // Durable snapshot before the committing event (#41).
            let publish_guard = snapshots::acquire_publish_guard()?;
            let snap = Some(snapshots::save_durable(
                &publish_guard,
                project.id,
                &hash,
                &content,
            )?);

            db.transaction(|db| {
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
                db.upsert_file_state(
                    project.id,
                    &path_str,
                    &hash,
                    true,
                    content.len() as i64,
                    mtime_nanos,
                )?;
                Ok(())
            })?;

            if verbose {
                eprintln!(
                    "  MODIFIED {}",
                    crate::relative_path(&path_str, &project.root_path)
                );
            }
        }
        _ => {
            return handle_create(db, project, path, len, mtime_nanos, verbose);
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
    let Some(path_str) = db_key(path) else {
        return Ok(());
    };

    let prev_hash = db
        .get_file_state(project.id, &path_str)?
        .and_then(|s| if s.exists_now { s.latest_hash } else { None });

    if prev_hash.is_none() {
        return Ok(());
    }

    // Event + mark_deleted commit together so a crash can't record the DELETED
    // event without flipping exists_now, or vice versa (#41).
    db.transaction(|db| {
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
        Ok(())
    })?;

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
    len: u64,
    mtime_nanos: Option<i64>,
    verbose: bool,
) -> Result<()> {
    // Both names must round-trip: a mangled key would mis-track the new file or
    // store a corrupt previous_path (#34). If either side is non-UTF8, skip — the
    // new file, if valid, is picked up by its next event or the next scan.
    let (Some(old_str), Some(new_str)) = (db_key(old_path), db_key(new_path)) else {
        return Ok(());
    };

    let dest_prev_hash = if old_str == new_str {
        None
    } else {
        db.get_file_state(project.id, &new_str)?
            .and_then(|s| if s.exists_now { s.latest_hash } else { None })
    };

    let content = match read_within_limit(new_path, len) {
        Some(c) => c,
        None => return Ok(()),
    };
    let hash = compute_hash(&content);

    let source_prev_hash = db
        .get_file_state(project.id, &old_str)?
        .and_then(|s| if s.exists_now { s.latest_hash } else { None });

    // Durable snapshot before the committing event (#41).
    let publish_guard = snapshots::acquire_publish_guard()?;
    let snap = Some(snapshots::save_durable(
        &publish_guard,
        project.id,
        &hash,
        &content,
    )?);

    // The RENAMED event, the old path's deletion, and the new path's state all
    // commit together so a crash can't split the rename across two autocommits (#41).
    db.transaction(|db| {
        if let Some(dest_hash) = dest_prev_hash
            .as_deref()
            .filter(|dest_hash| *dest_hash != hash.as_str())
        {
            db.insert_event(
                project.id,
                &new_str,
                "DELETED",
                None,
                Some(dest_hash),
                None,
                None,
                None,
            )?;
        }

        db.insert_event(
            project.id,
            &new_str,
            "RENAMED",
            Some(&hash),
            source_prev_hash.as_deref(),
            snap.as_deref(),
            Some(&old_str),
            Some(content.len() as i64),
        )?;
        db.mark_deleted(project.id, &old_str)?;
        db.upsert_file_state(
            project.id,
            &new_str,
            &hash,
            true,
            content.len() as i64,
            mtime_nanos,
        )?;
        Ok(())
    })?;

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

    /// Renaming one tracked file over another must preserve the overwritten
    /// destination's previous content as a DELETED event. Otherwise retention can
    /// orphan that snapshot once file_state is updated to the incoming content.
    #[test]
    fn handle_rename_records_deleted_event_for_overwritten_destination() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let tree = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(tree.path()).unwrap();

        let old_path = tree.path().join("incoming.txt");
        let new_path = tree.path().join("config.txt");
        let new_content = b"incoming content";
        std::fs::write(&new_path, new_content).unwrap();

        let old_str = db_key(&old_path).unwrap();
        let new_str = db_key(&new_path).unwrap();
        db.upsert_file_state(project.id, &old_str, "source_hash", true, 1, None)
            .unwrap();
        db.upsert_file_state(project.id, &new_str, "overwritten_hash", true, 1, None)
            .unwrap();

        handle_rename(
            &db,
            &project,
            &old_path,
            &new_path,
            new_content.len() as u64,
            None,
            false,
        )
        .unwrap();

        let new_hash = compute_hash(new_content);
        let deleted = db
            .get_latest_deleted_event(project.id, &new_str)
            .unwrap()
            .expect("overwritten destination should get a DELETED event");
        assert_eq!(deleted.previous_hash.as_deref(), Some("overwritten_hash"));

        let latest = db.get_latest_event(project.id, &new_str).unwrap().unwrap();
        assert_eq!(latest.event_type, "RENAMED");
        assert_eq!(latest.previous_hash.as_deref(), Some("source_hash"));
        assert_eq!(latest.current_hash.as_deref(), Some(new_hash.as_str()));

        let old_state = db.get_file_state(project.id, &old_str).unwrap().unwrap();
        assert!(!old_state.exists_now);
        let new_state = db.get_file_state(project.id, &new_str).unwrap().unwrap();
        assert_eq!(new_state.latest_hash.as_deref(), Some(new_hash.as_str()));

        let hashes = db.get_live_hashes(project.id).unwrap();
        assert!(
            hashes.contains("overwritten_hash"),
            "overwritten destination hash must stay live while its DELETED event survives: {:?}",
            hashes
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

        let err = initial_scan_with_limit(&db, &project, dir.path(), false, 5, true);
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
        initial_scan_with_limit(&db, &project, tree.path(), false, usize::MAX, true).unwrap();
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
                .filter_entry(|e| {
                    !should_ignore_with_type(e.path(), tree.path(), e.file_type().is_dir())
                })
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && !e.path_is_symlink())
                .map(|e| e.path().to_path_buf())
                .collect::<Vec<_>>()
        };

        // Phase A: read+hash via the live-path timed read (fs_with_timeout
        // spawns a thread per call), measuring that wrapper's overhead.
        let paths = collect_paths();
        let t = Instant::now();
        let mut acc = 0u8;
        for p in &paths {
            let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            if let Some(c) = read_within_limit(p, len) {
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

        let result = initial_scan_with_limit(&db, &project, dir.path(), false, 100, true);
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

        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX, true).unwrap();

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

        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX, true).unwrap();

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

    /// #34: a valid UTF-8 path becomes its own key; a non-UTF8 path is rejected
    /// (None) so it's skipped rather than stored under a lossily-mangled key that
    /// can't round-trip. Constructed purely from bytes — no filesystem — since
    /// macOS won't even let a non-UTF8 filename exist on disk.
    #[test]
    fn db_key_rejects_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(
            db_key(Path::new("/home/user/project/src/main.rs")).as_deref(),
            Some("/home/user/project/src/main.rs"),
            "a valid UTF-8 path must round-trip as its own key"
        );

        let non_utf8 = Path::new(OsStr::from_bytes(b"/home/user/project/\xff\xfe.rs"));
        assert!(
            db_key(non_utf8).is_none(),
            "a non-UTF8 path must be rejected, not lossily mangled"
        );
    }

    /// #28: the timeout-guarded stat returns the metadata for a real file. This is
    /// the happy path that runs on every live event after the guard was added.
    #[test]
    fn symlink_metadata_timeout_returns_metadata_for_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "hello").unwrap();
        let meta = symlink_metadata_timeout(&file).expect("metadata for existing file");
        assert!(meta.is_file());
        assert_eq!(meta.len(), 5);
    }

    /// #28: a missing path yields None — the same signal a *timed-out* stat
    /// produces. `process_event` treats both identically (skip the event), so a
    /// hung mount degrades to "skip + let the health check pause" rather than
    /// wedging the watch loop.
    #[test]
    fn symlink_metadata_timeout_returns_none_for_missing_path() {
        assert!(symlink_metadata_timeout(Path::new("/nonexistent/path/xyz")).is_none());
    }

    /// #29: the shared watchdog worker is reused across ops and must survive a
    /// panicking op. A bad op yields None for that call only — a subsequent op
    /// still completes, proving one panic can't disable fs timeouts for the rest
    /// of the process.
    #[test]
    fn fs_watchdog_survives_a_panicking_op_and_keeps_serving() {
        let panicked: Option<()> = fs_with_timeout(|| panic!("boom inside fs op"));
        assert!(panicked.is_none(), "a panicking op must return None");

        assert_eq!(
            fs_with_timeout(|| 7u32),
            Some(7),
            "the watchdog must keep serving after a panicking op"
        );
    }

    /// #26 fast path: when the recorded size and mtime both match, `handle_modify`
    /// must skip the read+hash entirely. We prove the skip by seeding a
    /// deliberately *wrong* hash with the file's real size+mtime — if the read
    /// happened, the true hash would differ and a MODIFIED event would appear.
    #[test]
    fn handle_modify_fast_path_skips_when_size_and_mtime_match() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let path_str = file.to_string_lossy().to_string();

        let meta = std::fs::metadata(&file).unwrap();
        let size = meta.len();
        let mtime = mtime_nanos(&meta);
        assert!(mtime.is_some(), "test requires a usable mtime");

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();
        db.upsert_file_state(
            project.id,
            &path_str,
            "deadbeefdeadbeef",
            true,
            size as i64,
            mtime,
        )
        .unwrap();

        handle_modify(&db, &project, &file, size, mtime, false).unwrap();

        assert!(
            db.get_latest_event(project.id, &path_str)
                .unwrap()
                .is_none(),
            "matching size+mtime must short-circuit before any read/hash"
        );
    }

    /// #26 fast path falls through on any mismatch: a stale recorded mtime forces
    /// the full read+hash, which detects the changed content and records MODIFIED.
    #[test]
    fn handle_modify_reads_when_mtime_differs() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"new content").unwrap();
        let path_str = file.to_string_lossy().to_string();

        let meta = std::fs::metadata(&file).unwrap();
        let size = meta.len();
        let mtime = mtime_nanos(&meta);

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();
        // Same size, but a stale mtime and a non-matching hash.
        let stale_mtime = mtime.map(|m| m - 1_000_000_000);
        db.upsert_file_state(
            project.id,
            &path_str,
            "0000000000000000",
            true,
            size as i64,
            stale_mtime,
        )
        .unwrap();

        handle_modify(&db, &project, &file, size, mtime, false).unwrap();

        let ev = db.get_latest_event(project.id, &path_str).unwrap();
        assert_eq!(
            ev.map(|e| e.event_type),
            Some("MODIFIED".to_string()),
            "mtime mismatch must fall through to a real read+hash"
        );
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

    /// A file tracked in the DB but absent from disk triggers a DELETED event and
    /// marks exists_now false — as long as the tree is otherwise intact (a real
    /// `keep.rs` survives so this isn't mistaken for an empty remount, #31).
    #[test]
    fn initial_scan_records_deletion_for_missing_file() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        // A surviving file keeps the tree non-empty, so the missing file reads as
        // a genuine deletion rather than a wholesale disappearance.
        std::fs::write(dir.path().join("keep.rs"), b"still here").unwrap();

        // Seed the DB with a file that no longer exists on disk.
        let phantom_path = dir.path().join("phantom.rs").to_string_lossy().to_string();
        db.upsert_file_state(project.id, &phantom_path, "deadbeef", true, 0, None)
            .unwrap();

        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX, true).unwrap();

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

    /// #31: when the entire watched tree comes back empty (e.g. a network share
    /// remounts empty), the scan must NOT record every tracked file as deleted —
    /// that would flip `exists_now` to 0 and let prune reclaim the snapshots,
    /// turning a transient mount event into permanent history loss. History is
    /// preserved: states stay alive and no DELETED events are written.
    #[test]
    fn initial_scan_skips_mass_deletion_when_tree_came_back_empty() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        // Seed several tracked-and-alive files; the dir on disk stays empty.
        for i in 0..5 {
            let p = dir
                .path()
                .join(format!("f{i}.rs"))
                .to_string_lossy()
                .to_string();
            db.upsert_file_state(project.id, &p, &format!("hash{i}"), true, 0, None)
                .unwrap();
        }

        // Even trusting deletions, an empty tree is treated as a mount anomaly.
        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX, true).unwrap();

        for i in 0..5 {
            let p = dir
                .path()
                .join(format!("f{i}.rs"))
                .to_string_lossy()
                .to_string();
            let state = db.get_file_state(project.id, &p).unwrap().unwrap();
            assert!(
                state.exists_now,
                "f{i}.rs must stay alive after empty remount"
            );
            assert!(
                db.get_latest_event(project.id, &p).unwrap().is_none(),
                "no DELETED event may be recorded for f{i}.rs"
            );
        }
    }

    /// #31: when deletions are untrusted (the resume path detected the root is a
    /// different filesystem object — a swapped disk), missing files are not
    /// recorded as deleted even though new files are present and recorded.
    #[test]
    fn initial_scan_skips_deletions_when_untrusted() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(dir.path()).unwrap();

        // An old tracked file (from the original disk) that isn't on this disk.
        let old = dir.path().join("old.rs").to_string_lossy().to_string();
        db.upsert_file_state(project.id, &old, "oldhash", true, 0, None)
            .unwrap();

        // A file that IS present on the swapped-in disk.
        std::fs::write(dir.path().join("new.rs"), b"foreign content").unwrap();

        // trust_deletions = false → the swapped disk's absence of old.rs must not
        // be recorded as a deletion.
        initial_scan_with_limit(&db, &project, dir.path(), false, usize::MAX, false).unwrap();

        let old_state = db.get_file_state(project.id, &old).unwrap().unwrap();
        assert!(old_state.exists_now, "old.rs history must be preserved");
        assert!(
            db.get_latest_event(project.id, &old).unwrap().is_none(),
            "no DELETED event may be recorded for old.rs"
        );

        // The present file is still recorded normally.
        let new = dir.path().join("new.rs").to_string_lossy().to_string();
        let new_ev = db.get_latest_event(project.id, &new).unwrap().unwrap();
        assert_eq!(new_ev.event_type, "CREATED");
    }
}
