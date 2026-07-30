use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::db::Database;
use crate::watcher;
use crate::{BOLD, GREEN, RED, RESET, YELLOW, backtrack_dir};

/// Derive a per-project PID file path from the project root.
/// Uses a truncated SHA-256 so each project gets its own file.
fn pid_file_for_root(bt_dir: &Path, root: &Path) -> PathBuf {
    let hash = Sha256::digest(root.to_string_lossy().as_bytes());
    let short = crate::to_hex(&hash[..8]);
    bt_dir.join("pids").join(format!("{}.pid", short))
}

pub fn is_recording(root: &Path) -> Result<bool> {
    let bt_dir = backtrack_dir()?;
    let pid_path = pid_file_for_root(&bt_dir, root);
    Ok(pid_path.exists() && is_daemon_alive(&pid_path))
}

/// Ensure continuous history is active before opening a Run. Returns true when
/// this call started the recorder and false when one was already running.
pub fn ensure_recording(root: &Path) -> Result<bool> {
    if is_recording(root)? {
        return Ok(false);
    }

    let bt_dir = backtrack_dir()?;
    let log_path = crate::logging::log_path(&bt_dir);
    let log_cursor = startup_log_cursor(&log_path);
    let executable = std::env::current_exe()?;
    let mut child = std::process::Command::new(executable)
        .arg("start")
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to start Undo recorder")?;

    let pid_path = pid_file_for_root(&bt_dir, root);
    wait_for_recorder_start(&mut child, &pid_path, &log_path, log_cursor)?;
    Ok(true)
}

#[derive(Clone, Copy, Default)]
struct StartupLogCursor {
    offset: u64,
    identity: Option<(u64, u64)>,
}

fn startup_log_cursor(log_path: &Path) -> StartupLogCursor {
    std::fs::metadata(log_path)
        .map(|metadata| StartupLogCursor {
            offset: metadata.len(),
            identity: Some((metadata.dev(), metadata.ino())),
        })
        .unwrap_or_default()
}

fn wait_for_recorder_start(
    child: &mut std::process::Child,
    pid_path: &Path,
    log_path: &Path,
    log_cursor: StartupLogCursor,
) -> Result<()> {
    for _ in 0..200 {
        if pid_path.exists() && is_daemon_alive(pid_path) {
            let ready = std::fs::read_to_string(pid_path)
                .ok()
                .and_then(|contents| contents.lines().nth(2).map(str::to_string))
                .is_some_and(|state| state == "ready");
            if ready {
                return Ok(());
            }
        }

        if let Some(status) = child
            .try_wait()
            .context("failed to check Undo recorder startup")?
        {
            if let Some(error) = startup_error_from_log(log_path, child.id(), log_cursor) {
                anyhow::bail!("{}", error);
            }
            anyhow::bail!(
                "Undo recorder exited during startup ({}).\nCheck the diagnostic log at: {}",
                status,
                log_path.display()
            );
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    anyhow::bail!(
        "Undo did not start recording within 10 seconds.\nCheck the diagnostic log at: {}",
        log_path.display()
    )
}

fn startup_error_from_log(log_path: &Path, pid: u32, cursor: StartupLogCursor) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(log_path).ok()?;
    let metadata = file.metadata().ok()?;
    // Rotation replaces the active log with a new inode. Read the replacement
    // from byte zero even if concurrent daemons have already grown it beyond
    // the old file's offset.
    let unchanged = cursor.identity == Some((metadata.dev(), metadata.ino()));
    let offset = if unchanged && metadata.len() >= cursor.offset {
        cursor.offset
    } else {
        0
    };
    file.seek(SeekFrom::Start(offset)).ok()?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    latest_process_error(&String::from_utf8_lossy(&bytes), pid)
}

fn latest_process_error(contents: &str, pid: u32) -> Option<String> {
    let marker = format!(" [{}] ERROR ", pid);
    let mut latest = None;
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        let Some(marker_start) = line.find(&marker) else {
            continue;
        };

        let mut message = line[marker_start + marker.len()..].to_string();
        while lines
            .peek()
            .is_some_and(|line| !is_structured_log_line(line))
        {
            message.push('\n');
            message.push_str(lines.next().expect("peeked log continuation"));
        }
        latest = Some(message);
    }

    latest.filter(|message| !message.trim().is_empty())
}

fn is_structured_log_line(line: &str) -> bool {
    let Some((timestamp, entry)) = line.split_once(" [") else {
        return false;
    };
    let Some((pid, message)) = entry.split_once("] ") else {
        return false;
    };
    let timestamp = timestamp.as_bytes();

    timestamp.len() == 23
        && timestamp[4] == b'-'
        && timestamp[7] == b'-'
        && timestamp[10] == b'T'
        && timestamp[13] == b':'
        && timestamp[16] == b':'
        && timestamp[19] == b'.'
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && ["INFO ", "WARN ", "ERROR "]
            .iter()
            .any(|level| message.starts_with(level))
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
            "Undo did not start: refusing to run as root because saved history belongs to the current user.\n\
             To bypass this check, rerun with --force; it also bypasses ownership, file-count, and overlap safety checks."
        );
    }
    Ok(())
}

/// Refuse to watch directories owned by root (uid 0) or system accounts
/// (uid < 500 on macOS, < 1000 on Linux). This prevents accidentally
/// watching /, /etc, /usr, /var, etc.
fn check_directory_ownership(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)?;
    check_directory_owner(path, meta.uid())
}

fn check_directory_owner(path: &Path, uid: u32) -> Result<()> {
    if uid == 0 {
        anyhow::bail!(
            "Undo did not start: folder '{}' is owned by root.\n\
             To bypass this ownership check, rerun with --force; it also bypasses file-count and overlap safety checks.",
            path.display()
        );
    }

    if uid < system_uid_threshold() {
        anyhow::bail!(
            "Undo did not start: folder '{}' is owned by a system account (uid {}).\n\
             To bypass this ownership check, rerun with --force; it also bypasses file-count and overlap safety checks.",
            path.display(),
            uid
        );
    }

    Ok(())
}

fn system_uid_threshold() -> u32 {
    if cfg!(target_os = "macos") { 500 } else { 1000 }
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
///
/// `exclude_pid` skips PID files belonging to the calling process. `cmd_start`
/// already locks its own pid file before this check runs, and on macOS / Linux
/// flock treats two open file descriptions in the same process as conflicting,
/// so `is_daemon_alive` would otherwise report our own pid file as alive and
/// the equal-paths branch below would self-reject every fresh `undo start`.
fn check_no_overlap(bt_dir: &Path, new_root: &Path, exclude_pid: u32) -> Result<()> {
    let new_str = new_root.to_string_lossy();
    for (pid, existing) in active_daemons(bt_dir) {
        if pid == exclude_pid {
            continue;
        }
        let ex_str = existing.to_string_lossy();

        let overlap = if new_str.len() >= ex_str.len() {
            // new_root is equal to or a child of existing
            new_str.starts_with(ex_str.as_ref())
                && (new_str.len() == ex_str.len() || new_str.as_bytes()[ex_str.len()] == b'/')
        } else {
            // new_root is a parent of existing
            ex_str.starts_with(new_str.as_ref()) && ex_str.as_bytes()[new_str.len()] == b'/'
        };

        if overlap {
            anyhow::bail!(
                "Undo did not start: this folder overlaps with one already being recorded.\n\
                 Recorded folder: {}\n\
                 To bypass this overlap check, rerun with --force; it also bypasses ownership and file-count safety checks. (Recorder PID: {})",
                existing.display(),
                pid,
            );
        }
    }
    Ok(())
}

pub fn cmd_start(verbose: bool, force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let bt_dir = backtrack_dir()?;

    // Route daemon errors and crashes to ~/.undo/undo.log before anything else
    // can fail, so even early startup problems leave a trace.
    crate::logging::init(&bt_dir);
    crate::logging::install_panic_hook();

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
        // Do not truncate on open: if a live daemon holds the lock we read the
        // existing PID/path back; we only clear it (set_len(0)) after we win it.
        .truncate(false)
        .open(&pid_path)?;

    if !try_lock_exclusive(&pid_file) {
        use std::io::Read;
        let mut contents = String::new();
        pid_file.read_to_string(&mut contents)?;
        let pid = contents.lines().next().unwrap_or("?");
        let project = contents.lines().nth(1).unwrap_or("unknown");
        println!("Undo is already recording file changes for this folder.");
        println!("Folder: {} (recorder PID: {})", project, pid);
        return Ok(());
    }

    // Lock acquired — write our PID.
    write_pid_state(&mut pid_file, &cwd, "starting")?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&pid_path, std::fs::Permissions::from_mode(0o600));
    }

    if !force {
        check_no_overlap(&bt_dir, &cwd, std::process::id())?;
    }

    let db = Database::open()?;
    let project = db.get_or_create_project(&cwd)?;

    // Catch SIGINT / SIGTERM so we clean up the PID file.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;

    crate::ignore::init(&cwd);

    crate::log_info!(
        "daemon started (PID {}) watching {}",
        std::process::id(),
        cwd.display()
    );

    println!("{}Undo is recording file changes.{}", BOLD, RESET);
    println!("Folder: {}", cwd.display());
    println!();

    watcher::initial_scan(&db, &project, &cwd, verbose, force)?;
    write_pid_state(&mut pid_file, &cwd, "ready")?;

    let retention_cfg = crate::retention::load_config(Some(&cwd));
    match crate::retention::prune(&db, project.id, &retention_cfg, false) {
        Ok(stats) if stats.events_deleted + stats.snapshots_deleted + stats.backups_deleted > 0 => {
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

    // Startup integrity pass: a SHALLOW (existence-only) check that every snapshot
    // referenced by surviving history is present on disk. Report-only (never
    // deletes), so a power-loss-torn or hand-deleted snapshot is surfaced in the log
    // instead of failing later at restore time (#41). Kept shallow so startup stays a
    // stat-per-hash; the deep decompress/CRC check runs on demand in `undo status`.
    match crate::integrity::verify_project(&db, project.id, false, true) {
        Ok(report) if report.missing > 0 => crate::log_warn!(
            "integrity: {} of {} referenced snapshots missing on disk — affected versions \
             cannot be restored (run `undo status` for a deep decompress check)",
            report.missing,
            report.checked,
        ),
        Ok(report) => crate::log_info!(
            "integrity: {} referenced snapshots present (existence check)",
            report.checked
        ),
        Err(e) => crate::log_warn!("integrity check failed: {}", e),
    }

    // pid_file (and its lock) stays alive for the duration of the watch loop.
    watcher::watch_directory(&db, &project, &cwd, shutdown, verbose)?;

    let _ = std::fs::remove_file(&pid_path);
    drop(pid_file);
    crate::log_info!("daemon stopped (PID {})", std::process::id());
    eprintln!("\nUndo stopped.");

    Ok(())
}

fn write_pid_state(file: &mut std::fs::File, root: &Path, state: &str) -> Result<()> {
    use std::io::{Seek, Write};
    file.set_len(0)?;
    file.rewind()?;
    write!(
        file,
        "{}\n{}\n{}",
        std::process::id(),
        root.display(),
        state
    )?;
    file.sync_data()?;
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
        println!("Undo is not recording file changes for this folder.");
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
        println!("Undo was not recording file changes.");
        println!("Removed a stale recorder marker.");
        std::fs::remove_file(pid_path)?;
        return Ok(());
    }

    // Lock is held by a live undo daemon — safe to signal this PID. Use a
    // direct kill(2) syscall rather than spawning `/usr/bin/kill`: it avoids a
    // process fork, removes the PATH/external-binary dependency, and makes the
    // exact signal explicit.
    signal_terminate(pid);

    for _ in 0..60 {
        if !is_daemon_alive(pid_path) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    if is_daemon_alive(pid_path) {
        anyhow::bail!(
            "Undo did not stop recording within 6 seconds (recorder PID: {}).\n\
             If the recorder is stuck, stop it with: kill -9 {}",
            pid,
            pid
        );
    }

    let _ = std::fs::remove_file(pid_path);

    let project = contents.lines().nth(1).unwrap_or("unknown");
    println!("Undo stopped recording file changes.");
    println!("Folder: {} (recorder PID: {})", project, pid);
    Ok(())
}

fn stop_all_daemons(bt_dir: &Path) -> Result<()> {
    let pids_dir = bt_dir.join("pids");
    if !pids_dir.exists() {
        println!("Undo is not recording file changes in any folder.");
        return Ok(());
    }

    let mut stopped = 0;
    for entry in std::fs::read_dir(&pids_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("pid")
            && stop_one_daemon(&path).is_ok()
        {
            stopped += 1;
        }
    }

    if stopped == 0 {
        println!("Undo was not recording file changes in any folder.");
    }
    Ok(())
}

pub fn cmd_status() -> Result<()> {
    let bt_dir = backtrack_dir()?;
    let db = Database::open()?;
    let cwd = std::env::current_dir()?.canonicalize()?;

    println!("{}Undo recording status{}", BOLD, RESET);
    println!();

    match db.find_project_for_path(&cwd)? {
        Some(project) => {
            println!("Folder:        {}", project.root_path);

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
            println!("Recording:     {}", daemon_status);

            let db_path = bt_dir.join("database.db");
            let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
            println!(
                "Data index:    {} ({:.1} KB)",
                db_path.display(),
                db_size as f64 / 1024.0
            );

            let event_count = db.count_events(project.id)?;
            let snapshot_count = crate::snapshots::count(project.id)?;
            println!("File changes:  {}", event_count);
            println!("Saved versions: {}", snapshot_count);

            // Deep check (decompress/CRC) since the user is here and asking — this is
            // the expensive read-per-snapshot pass the daemon startup deliberately
            // skips. Silent per-problem (log_problems = false) so status shows one
            // summary line rather than a warning per bad snapshot (#41).
            let integrity =
                crate::integrity::verify_project(&db, project.id, true, false).unwrap_or_default();
            let integrity_line = if integrity.is_clean() {
                format!("{}OK{} ({} verified)", GREEN, RESET, integrity.checked)
            } else {
                format!(
                    "{}{} unreadable{} ({} missing, {} corrupt, of {} checked)",
                    RED,
                    integrity.problems(),
                    RESET,
                    integrity.missing,
                    integrity.corrupt,
                    integrity.checked,
                )
            };
            println!("History health: {}", integrity_line);

            let project_root = std::path::Path::new(&project.root_path);
            let cfg = crate::retention::load_config(Some(project_root));
            println!(
                "Keep history:  {} days, {} max",
                cfg.retention_days,
                crate::retention::format_size(cfg.max_size_bytes()),
            );

            // Single tree walk instead of three (was: dir_size x2 + total).
            let usage = crate::retention::disk_usage_breakdown().unwrap_or_default();
            println!(
                "Storage:       {} (versions: {}, backups: {}, index: {})",
                crate::retention::format_size(usage.total),
                crate::retention::format_size(usage.snapshots),
                crate::retention::format_size(usage.backups),
                crate::retention::format_size(db_size),
            );

            println!(
                "Diagnostic log: {}",
                crate::logging::log_path(&bt_dir).display()
            );
        }
        None => {
            println!("Undo is not recording file changes for this folder.");
            println!("Run {}undo start{} to begin.", BOLD, RESET);
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

/// Send SIGTERM to `pid` via a direct syscall. Returns true if the signal was
/// delivered. A failure (e.g. the process already exited, ESRCH) is not fatal —
/// the caller polls liveness afterward to confirm the daemon is gone.
fn signal_terminate(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
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

    /// `signal_terminate` must actually deliver SIGTERM via the syscall: spawn a
    /// long-lived child, signal it, and confirm it exits. Proves the libc::kill
    /// path is a real replacement for shelling out to `/usr/bin/kill`.
    #[test]
    fn signal_terminate_stops_a_live_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        assert!(
            signal_terminate(pid),
            "kill() should report success for a live pid"
        );

        // Wait briefly for the default SIGTERM disposition to take effect.
        let mut exited = false;
        for _ in 0..50 {
            if child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(exited, "child should have terminated after SIGTERM");
        let _ = child.wait();
    }

    /// A root-owned watch target must be rejected without depending on the
    /// ownership mapping of the host running the test.
    #[test]
    fn rejects_root_owned_directory() {
        let err = check_directory_owner(Path::new("/usr"), 0).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("owned by root") || msg.contains("system account"));
    }

    /// The current user's home directory is always a valid watch target.
    #[test]
    fn accepts_user_owned_directory() {
        let home = dirs::home_dir().expect("home dir");
        assert!(check_directory_ownership(&home).is_ok());
    }

    /// UIDs below the platform's normal-user threshold identify system accounts,
    /// independent of how a CI runner maps ownership for host paths.
    #[test]
    fn rejects_system_account_owned_directory() {
        let system_uid = system_uid_threshold() - 1;
        let err = check_directory_owner(Path::new("/system"), system_uid).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("system account") && msg.contains(&system_uid.to_string()),
            "expected ownership rejection, got: {}",
            msg
        );
    }

    #[test]
    fn accepts_normal_user_uid_at_platform_threshold() {
        assert!(check_directory_owner(Path::new("/home/user"), system_uid_threshold()).is_ok());
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
            .truncate(false)
            .open(&path)
            .unwrap();
        assert!(try_lock_exclusive(&file), "failed to lock test PID file");
        use std::io::Write;
        write!(&file, "{}\n{}", std::process::id(), root).unwrap();
        file
    }

    /// A pid value distinct from the caller; passed as `exclude_pid` so the
    /// helper's self-skip never accidentally matches the foreign daemon
    /// fixture and hides a real overlap.
    const FOREIGN_PID: u32 = u32::MAX;

    /// Starting a watcher inside an already-watched tree would produce duplicate events.
    #[test]
    fn overlap_rejects_child_of_watched_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo");

        let err = check_no_overlap(bt, Path::new("/foo/bar"), FOREIGN_PID).unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{}", err);
    }

    /// Starting a watcher that contains an existing watched subtree would cause double-recording.
    #[test]
    fn overlap_rejects_parent_of_watched_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        let err = check_no_overlap(bt, Path::new("/foo"), FOREIGN_PID).unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{}", err);
    }

    /// Re-watching the exact same directory must be rejected.
    #[test]
    fn overlap_rejects_exact_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        let err = check_no_overlap(bt, Path::new("/foo/bar"), FOREIGN_PID).unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{}", err);
    }

    /// Sibling directories have no overlap and must both be allowed.
    #[test]
    fn overlap_allows_sibling_directories() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        assert!(check_no_overlap(bt, Path::new("/foo/baz"), FOREIGN_PID).is_ok());
    }

    /// A directory whose name starts with an existing root's name must not be falsely rejected.
    #[test]
    fn overlap_no_false_positive_for_shared_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        let _lock = write_live_pid_file(bt, "/foo/bar");

        // "/foo/bar-extra" shares the string prefix but is not a subdirectory
        assert!(check_no_overlap(bt, Path::new("/foo/bar-extra"), FOREIGN_PID).is_ok());
    }

    /// With no active daemons, any directory is a valid watch target.
    #[test]
    fn overlap_passes_when_no_daemons_running() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();

        assert!(check_no_overlap(bt, Path::new("/any/path"), FOREIGN_PID).is_ok());
    }

    /// `cmd_start` locks its own pid file before running the overlap check.
    /// On macOS / Linux flock treats two open file descriptions in the same
    /// process as conflicting, so `is_daemon_alive` reports our own pid file
    /// as alive. Without an `exclude_pid` filter, the equal-paths branch of
    /// the overlap check would then self-reject every fresh `undo start`.
    /// This test pins that fix: writing the *current* process's pid into the
    /// pid file must NOT cause `check_no_overlap` to report an overlap.
    #[test]
    fn overlap_excludes_calling_process() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();

        // Write a pid file owned by the current process, holding a live flock —
        // mirrors the cmd_start sequence: lock our pid file, then check overlap.
        let root = "/proj/self";
        let path = pid_file_for_root(bt, Path::new(root));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        assert!(try_lock_exclusive(&file));
        use std::io::Write;
        write!(&file, "{}\n{}", std::process::id(), root).unwrap();

        // Must succeed: the only "live daemon" is us, and we're excluded.
        check_no_overlap(bt, Path::new(root), std::process::id())
            .expect("check_no_overlap must skip the calling process");
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

    #[test]
    fn startup_wait_surfaces_the_childs_logged_error_without_timing_out() {
        use std::io::Write;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("undo.log");
        let pid_path = dir.path().join("recorder.pid");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.05; exit 17"])
            .spawn()
            .unwrap();
        let child_pid = child.id();
        let mut log = std::fs::File::create(&log_path).unwrap();
        writeln!(log, "2026-07-20T20:24:00.000 [999] ERROR unrelated failure").unwrap();
        writeln!(
            log,
            "2026-07-20T20:24:00.001 [{}] ERROR Undo did not start: test rejection.\n\
             Use the reported reason.",
            child_pid
        )
        .unwrap();

        let started = Instant::now();
        let error = wait_for_recorder_start(
            &mut child,
            &pid_path,
            &log_path,
            StartupLogCursor::default(),
        )
        .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "early exit should not wait for the ten-second timeout"
        );
        assert_eq!(
            error.to_string(),
            "Undo did not start: test rejection.\nUse the reported reason."
        );
    }

    #[test]
    fn startup_error_reader_resets_its_boundary_after_log_rotation() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("undo.log");
        std::fs::write(&log_path, vec![b'x'; 256]).unwrap();
        let cursor = startup_log_cursor(&log_path);

        std::fs::rename(&log_path, dir.path().join("undo.log.1")).unwrap();
        let mut replacement = std::fs::File::create(&log_path).unwrap();
        writeln!(
            replacement,
            "2026-07-20T20:24:00.001 [42] ERROR current startup failure"
        )
        .unwrap();
        writeln!(
            replacement,
            "2026-07-20T20:24:00.002 [999] INFO {}",
            "x".repeat(300)
        )
        .unwrap();

        assert_eq!(
            startup_error_from_log(&log_path, 42, cursor).as_deref(),
            Some("current startup failure")
        );
    }

    #[test]
    fn recorder_readiness_wins_over_an_already_exited_child() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("undo.log");
        let pid_path = dir.path().join("recorder.pid");
        let mut pid_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&pid_path)
            .unwrap();
        assert!(try_lock_exclusive(&pid_file));
        write!(pid_file, "123\n/project\nready").unwrap();

        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 1"])
            .spawn()
            .unwrap();
        child.wait().unwrap();

        wait_for_recorder_start(
            &mut child,
            &pid_path,
            &log_path,
            StartupLogCursor::default(),
        )
        .expect("ready recorder state should be checked before child exit");
    }
}
