use anyhow::Result;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::db::Database;
use crate::watcher;
use crate::{backtrack_dir, BOLD, GREEN, RED, RESET, YELLOW};

/// Derive a per-project PID file path from the project root.
/// Uses a truncated SHA-256 so each project gets its own file.
fn pid_file_for_root(bt_dir: &Path, root: &Path) -> PathBuf {
    let hash = Sha256::digest(root.to_string_lossy().as_bytes());
    let short: String = hash.iter().take(8).map(|b| format!("{:02x}", b)).collect();
    bt_dir.join("pids").join(format!("{}.pid", short))
}

/// Migrate the old singleton `~/.undo/pid` to the new per-project layout.
/// Called once at the top of `cmd_start` and `cmd_stop` so old daemons
/// are visible after an upgrade.
fn migrate_old_pid_file(bt_dir: &Path) -> Result<()> {
    let old_pid = bt_dir.join("pid");
    if !old_pid.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(&old_pid)?;
    if let Some(root_line) = contents.lines().nth(1) {
        let root = Path::new(root_line);
        let new_path = pid_file_for_root(bt_dir, root);
        if !new_path.exists() {
            std::fs::write(&new_path, &contents)?;
        }
    }
    let _ = std::fs::remove_file(&old_pid);
    Ok(())
}

/// Refuse to run as root/sudo. The data directory resolves via $HOME,
/// so running as root silently writes to root's home, making snapshots
/// invisible to the normal user.
fn check_not_root() -> Result<()> {
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        anyhow::bail!(
            "refusing to run as root — undo stores data in the current user's home directory.\n\
             Running as root would write to root's home, making data invisible to your normal user.\n\
             Use --force to override."
        );
    }
    Ok(())
}

/// Refuse to watch directories owned by root (uid 0) or system accounts
/// (uid < 500 on macOS, < 1000 on Linux). This prevents accidentally
/// watching /, /etc, /usr, /var, etc.
fn check_directory_ownership(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)?;
    let uid = meta.uid();

    if uid == 0 {
        anyhow::bail!(
            "refusing to watch '{}': directory is owned by root.\n\
             Watching root-owned directories can be dangerous.\n\
             Use --force to override this check.",
            path.display()
        );
    }

    let system_uid_threshold = if cfg!(target_os = "macos") { 500 } else { 1000 };
    if uid < system_uid_threshold {
        anyhow::bail!(
            "refusing to watch '{}': directory is owned by a system account (uid {}).\n\
             Use --force to override this check.",
            path.display(),
            uid
        );
    }

    Ok(())
}

/// Return all (pid, root_path) pairs from PID files whose daemons
/// are genuinely alive (verified via flock, not just PID existence).
fn active_daemons(bt_dir: &Path) -> Vec<(u32, PathBuf)> {
    let pids_dir = bt_dir.join("pids");
    let Ok(entries) = std::fs::read_dir(&pids_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("pid"))
        .filter_map(|e| {
            if !is_daemon_alive(&e.path()) {
                return None;
            }
            let contents = std::fs::read_to_string(e.path()).ok()?;
            let mut lines = contents.lines();
            let pid: u32 = lines.next()?.parse().ok()?;
            let root = PathBuf::from(lines.next()?);
            Some((pid, root))
        })
        .collect()
}

/// Refuse to start if another daemon is already watching a parent or child
/// of `new_root`. Overlapping watchers cause duplicate events and wasted
/// snapshots because both daemons receive the same filesystem notifications.
fn check_no_overlap(bt_dir: &Path, new_root: &Path) -> Result<()> {
    let new_str = new_root.to_string_lossy();
    for (pid, existing) in active_daemons(bt_dir) {
        let ex_str = existing.to_string_lossy();

        let overlap = if new_str.len() >= ex_str.len() {
            // new_root is equal to or a child of existing
            new_str.starts_with(ex_str.as_ref())
                && (new_str.len() == ex_str.len()
                    || new_str.as_bytes()[ex_str.len()] == b'/')
        } else {
            // new_root is a parent of existing
            ex_str.starts_with(new_str.as_ref())
                && ex_str.as_bytes()[new_str.len()] == b'/'
        };

        if overlap {
            anyhow::bail!(
                "directory overlaps with an already-watched path.\n\
                 Running daemon (PID {}) is watching: {}\n\
                 Overlapping watchers cause duplicate events.\n\
                 Use --force to override.",
                pid,
                existing.display(),
            );
        }
    }
    Ok(())
}

pub fn cmd_start(verbose: bool, force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let bt_dir = backtrack_dir()?;

    if !force {
        check_not_root()?;
        check_directory_ownership(&cwd)?;
    }

    migrate_old_pid_file(&bt_dir)?;

    let pid_path = pid_file_for_root(&bt_dir, &cwd);

    // Open (or create) the PID file, then try an exclusive flock.
    // If we can't acquire the lock, a live daemon already holds it.
    let mut pid_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&pid_path)?;

    if !try_lock_exclusive(&pid_file) {
        use std::io::Read;
        let mut contents = String::new();
        pid_file.read_to_string(&mut contents)?;
        let pid = contents.lines().next().unwrap_or("?");
        let project = contents.lines().nth(1).unwrap_or("unknown");
        println!("undo is already running (PID {}).", pid);
        println!("Watching: {}", project);
        return Ok(());
    }

    // Lock acquired — write our PID.
    {
        use std::io::Write;
        pid_file.set_len(0)?;
        write!(pid_file, "{}\n{}", std::process::id(), cwd.display())?;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&pid_path, std::fs::Permissions::from_mode(0o600));
    }

    if !force {
        check_no_overlap(&bt_dir, &cwd)?;
    }

    let db = Database::open()?;
    let project = db.get_or_create_project(&cwd)?;

    // Catch SIGINT / SIGTERM so we clean up the PID file.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;

    crate::ignore::init(&cwd);

    println!("{}undo{} — filesystem history", BOLD, RESET);
    println!("Watching: {}", cwd.display());
    println!("Recording changes...");
    println!();

    watcher::initial_scan(&db, &project, &cwd, verbose, force)?;

    let retention_cfg = crate::retention::load_config(Some(&cwd));
    match crate::retention::prune(&db, project.id, &retention_cfg, false) {
        Ok(stats) if stats.events_deleted + stats.snapshots_deleted + stats.backups_deleted > 0 => {
            eprintln!(
                "{}auto-prune:{} removed {} events, {} snapshots, {} backups (freed {})",
                YELLOW, RESET,
                stats.events_deleted, stats.snapshots_deleted, stats.backups_deleted,
                crate::retention::format_size(stats.bytes_freed),
            );
        }
        Err(e) => eprintln!("{}warning:{} auto-prune failed: {}", YELLOW, RESET, e),
        _ => {}
    }

    // pid_file (and its lock) stays alive for the duration of the watch loop.
    watcher::watch_directory(&db, &project, &cwd, shutdown, verbose)?;

    let _ = std::fs::remove_file(&pid_path);
    drop(pid_file);
    eprintln!("\nundo stopped.");

    Ok(())
}

pub fn cmd_stop(all: bool) -> Result<()> {
    let bt_dir = backtrack_dir()?;

    migrate_old_pid_file(&bt_dir)?;

    if all {
        return stop_all_daemons(&bt_dir);
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let pid_path = pid_file_for_root(&bt_dir, &cwd);

    if !pid_path.exists() {
        println!("No undo daemon is running for this directory.");
        return Ok(());
    }

    stop_one_daemon(&pid_path)
}

fn stop_one_daemon(pid_path: &Path) -> Result<()> {
    let contents = std::fs::read_to_string(pid_path)?;
    let pid: u32 = contents
        .lines()
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid PID file"))?;

    if !is_daemon_alive(pid_path) {
        println!("Daemon is not running (stale PID file). Cleaning up.");
        std::fs::remove_file(pid_path)?;
        return Ok(());
    }

    // Lock is held by a live undo daemon — safe to signal this PID.
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;

    for _ in 0..60 {
        if !is_daemon_alive(pid_path) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(pid_path);

    let project = contents.lines().nth(1).unwrap_or("unknown");
    println!("undo daemon stopped (PID {}, {}).", pid, project);
    Ok(())
}

fn stop_all_daemons(bt_dir: &Path) -> Result<()> {
    let pids_dir = bt_dir.join("pids");
    if !pids_dir.exists() {
        println!("No undo daemons are running.");
        return Ok(());
    }

    let mut stopped = 0;
    for entry in std::fs::read_dir(&pids_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("pid") {
            if stop_one_daemon(&path).is_ok() {
                stopped += 1;
            }
        }
    }

    if stopped == 0 {
        println!("No undo daemons were running.");
    }
    Ok(())
}

pub fn cmd_status() -> Result<()> {
    let bt_dir = backtrack_dir()?;
    let db = Database::open()?;
    let cwd = std::env::current_dir()?.canonicalize()?;

    println!("{}undo{} — status", BOLD, RESET);
    println!();

    match db.find_project_for_path(&cwd)? {
        Some(project) => {
            println!("Project:   {}", project.root_path);

            let project_root = Path::new(&project.root_path);
            let pid_path = pid_file_for_root(&bt_dir, project_root);
            let daemon_status = if pid_path.exists() {
                if is_daemon_alive(&pid_path) {
                    let contents = std::fs::read_to_string(&pid_path).unwrap_or_default();
                    let pid = contents.lines().next().unwrap_or("?");
                    format!("{}running{} (PID {})", GREEN, RESET, pid)
                } else {
                    format!("{}not running{} (stale PID)", YELLOW, RESET)
                }
            } else {
                format!("{}not running{}", RED, RESET)
            };
            println!("Daemon:    {}", daemon_status);

            let db_path = bt_dir.join("database.db");
            let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
            println!(
                "Database:  {} ({:.1} KB)",
                db_path.display(),
                db_size as f64 / 1024.0
            );

            let event_count = db.count_events(project.id)?;
            let snapshot_count = crate::snapshots::count(project.id)?;
            println!("Events:    {}", event_count);
            println!("Snapshots: {}", snapshot_count);

            let project_root = std::path::Path::new(&project.root_path);
            let cfg = crate::retention::load_config(Some(project_root));
            println!(
                "Retention: {} days, {} max",
                cfg.retention_days,
                crate::retention::format_size(cfg.max_size_mb * 1024 * 1024),
            );

            let snap_size = crate::retention::dir_size("snapshots").unwrap_or(0);
            let backup_size = crate::retention::dir_size("backups").unwrap_or(0);
            let total = crate::retention::total_disk_usage().unwrap_or(0);
            println!(
                "Disk:      {} (snapshots: {}, backups: {}, db: {})",
                crate::retention::format_size(total),
                crate::retention::format_size(snap_size),
                crate::retention::format_size(backup_size),
                crate::retention::format_size(db_size),
            );
        }
        None => {
            println!("No project being watched for this directory.");
            println!(
                "Run {}undo start{} to begin watching.",
                BOLD, RESET
            );
        }
    }

    Ok(())
}

/// Try to acquire an exclusive, non-blocking flock on an open file.
/// Returns true if the lock was acquired (caller now holds it until the
/// file is dropped), false if another process already holds it.
fn try_lock_exclusive(file: &std::fs::File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Probe whether a daemon is alive by trying to lock its PID file.
/// If we can acquire the lock the daemon is dead; the lock is released
/// when the probing File handle is dropped.
fn is_daemon_alive(pid_path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(pid_path) else {
        return false;
    };
    !try_lock_exclusive(&file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Watching /usr or similar root-owned paths would silently snapshot system files;
    /// the ownership check must block it.
    #[test]
    fn rejects_root_owned_directory() {
        let err = check_directory_ownership(Path::new("/usr")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("owned by root") || msg.contains("system account"));
    }

    /// The current user's home directory is always a valid watch target.
    #[test]
    fn accepts_user_owned_directory() {
        let home = dirs::home_dir().expect("home dir");
        assert!(check_directory_ownership(&home).is_ok());
    }

    /// /etc is root-owned; verifies the ownership check fires for another system path.
    #[test]
    fn rejects_etc_directory() {
        let err = check_directory_ownership(Path::new("/etc")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("root") || msg.contains("system account"),
            "expected ownership rejection, got: {}",
            msg
        );
    }

    /// Running undo as root is disallowed; verifies the check passes for a normal test process.
    #[test]
    fn check_not_root_passes_for_normal_user() {
        // Tests run as a normal user, so this should succeed.
        assert!(check_not_root().is_ok());
    }

    /// Two different roots must map to distinct PID files so each daemon can be tracked independently.
    #[test]
    fn pid_files_are_unique_per_project() {
        let bt_dir = Path::new("/tmp/undo-test-pids");
        let a = pid_file_for_root(bt_dir, Path::new("/home/user/project-a"));
        let b = pid_file_for_root(bt_dir, Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    /// The same root must produce the same PID file path across multiple calls.
    #[test]
    fn pid_file_is_stable_for_same_root() {
        let bt_dir = Path::new("/tmp/undo-test-pids");
        let root = Path::new("/home/user/project");
        let first = pid_file_for_root(bt_dir, root);
        let second = pid_file_for_root(bt_dir, root);
        assert_eq!(first, second);
    }

    /// The legacy single-file pid format must be migrated to the per-project layout on startup.
    #[test]
    fn migrate_old_pid_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();

        let old_pid = bt.join("pid");
        let root = "/home/user/project";
        std::fs::write(&old_pid, format!("12345\n{}", root)).unwrap();

        migrate_old_pid_file(bt).unwrap();

        assert!(!old_pid.exists(), "old pid file should be removed");
        let new_pid = pid_file_for_root(bt, Path::new(root));
        assert!(new_pid.exists(), "new per-project pid file should exist");
        let contents = std::fs::read_to_string(&new_pid).unwrap();
        assert!(contents.contains("12345"));
        assert!(contents.contains(root));
    }

    /// Migration is safe to call when no legacy pid file exists.
    #[test]
    fn migrate_is_noop_when_no_old_pid() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        assert!(migrate_old_pid_file(bt).is_ok());
    }

    // ── overlap detection ───────────────────────────────────────────

    /// Create a PID file and hold an exclusive flock on it so
    /// `is_daemon_alive` returns true. Caller must keep the returned
    /// File alive for the duration of the test.
    fn write_live_pid_file(bt_dir: &Path, root: &str) -> std::fs::File {
        let path = pid_file_for_root(bt_dir, Path::new(root));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(try_lock_exclusive(&file), "failed to lock test PID file");
        use std::io::Write;
        write!(&file, "{}\n{}", std::process::id(), root).unwrap();
        file
    }

    /// Starting a watcher inside an already-watched tree would produce duplicate events.
    #[test]
    fn overlap_rejects_child_of_watched_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo");

        let err = check_no_overlap(bt, Path::new("/foo/bar")).unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{}", err);
    }

    /// Starting a watcher that contains an existing watched subtree would cause double-recording.
    #[test]
    fn overlap_rejects_parent_of_watched_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        let err = check_no_overlap(bt, Path::new("/foo")).unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{}", err);
    }

    /// Re-watching the exact same directory must be rejected.
    #[test]
    fn overlap_rejects_exact_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        let err = check_no_overlap(bt, Path::new("/foo/bar")).unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{}", err);
    }

    /// Sibling directories have no overlap and must both be allowed.
    #[test]
    fn overlap_allows_sibling_directories() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        assert!(check_no_overlap(bt, Path::new("/foo/baz")).is_ok());
    }

    /// A directory whose name starts with an existing root's name must not be falsely rejected.
    #[test]
    fn overlap_no_false_positive_for_shared_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        // "/foo/bar-extra" shares the string prefix but is not a subdirectory
        assert!(check_no_overlap(bt, Path::new("/foo/bar-extra")).is_ok());
    }

    /// With no active daemons, any directory is a valid watch target.
    #[test]
    fn overlap_passes_when_no_daemons_running() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();

        assert!(check_no_overlap(bt, Path::new("/any/path")).is_ok());
    }

    /// A PID file not held by any process (no flock) is stale and must be ignored.
    #[test]
    fn stale_pid_file_detected_without_lock() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let path = pid_file_for_root(bt, Path::new("/some/project"));
        std::fs::write(&path, "99999\n/some/project").unwrap();

        assert!(!is_daemon_alive(&path), "unlocked PID file should be stale");
    }

    /// A PID file held by a live process (exclusive flock) must be reported as alive.
    #[test]
    fn locked_pid_file_detected_as_alive() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/some/project");

        let path = pid_file_for_root(bt, Path::new("/some/project"));
        assert!(is_daemon_alive(&path), "locked PID file should be alive");
    }
}
