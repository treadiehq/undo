use anyhow::Result;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
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

pub fn cmd_start(verbose: bool, force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let bt_dir = backtrack_dir()?;

    if !force {
        check_not_root()?;
        check_directory_ownership(&cwd)?;
    }

    migrate_old_pid_file(&bt_dir)?;

    let pid_path = pid_file_for_root(&bt_dir, &cwd);

    // Refuse to start if daemon is already running for this project.
    if pid_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&pid_path) {
            if let Some(pid) = contents.lines().next().and_then(|s| s.parse::<u32>().ok()) {
                if is_pid_running(pid) {
                    let project = contents.lines().nth(1).unwrap_or("unknown");
                    println!("undo is already running (PID {}).", pid);
                    println!("Watching: {}", project);
                    return Ok(());
                }
            }
        }
        let _ = std::fs::remove_file(&pid_path);
    }

    let db = Database::open()?;
    let project = db.get_or_create_project(&cwd)?;

    // Write PID file: line 1 = pid, line 2 = project root
    let pid = std::process::id();
    std::fs::write(&pid_path, format!("{}\n{}", pid, cwd.display()))?;

    // Catch SIGINT / SIGTERM so we clean up the PID file.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;

    println!("{}undo{} — filesystem history", BOLD, RESET);
    println!("Watching: {}", cwd.display());
    println!("Recording changes...");
    println!();

    watcher::initial_scan(&db, &project, &cwd, verbose, force)?;
    watcher::watch_directory(&db, &project, &cwd, shutdown, verbose)?;

    let _ = std::fs::remove_file(&pid_path);
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

    if !is_pid_running(pid) {
        println!("Daemon is not running (stale PID file). Cleaning up.");
        std::fs::remove_file(pid_path)?;
        return Ok(());
    }

    std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;

    std::thread::sleep(std::time::Duration::from_millis(500));
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
                let contents = std::fs::read_to_string(&pid_path).unwrap_or_default();
                let pid: u32 = contents
                    .lines()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if pid > 0 && is_pid_running(pid) {
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

fn is_pid_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_root_owned_directory() {
        let err = check_directory_ownership(Path::new("/usr")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("owned by root") || msg.contains("system account"));
    }

    #[test]
    fn accepts_user_owned_directory() {
        let home = dirs::home_dir().expect("home dir");
        assert!(check_directory_ownership(&home).is_ok());
    }

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

    #[test]
    fn check_not_root_passes_for_normal_user() {
        // Tests run as a normal user, so this should succeed.
        assert!(check_not_root().is_ok());
    }

    #[test]
    fn pid_files_are_unique_per_project() {
        let bt_dir = Path::new("/tmp/undo-test-pids");
        let a = pid_file_for_root(bt_dir, Path::new("/home/user/project-a"));
        let b = pid_file_for_root(bt_dir, Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn pid_file_is_stable_for_same_root() {
        let bt_dir = Path::new("/tmp/undo-test-pids");
        let root = Path::new("/home/user/project");
        let first = pid_file_for_root(bt_dir, root);
        let second = pid_file_for_root(bt_dir, root);
        assert_eq!(first, second);
    }

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

    #[test]
    fn migrate_is_noop_when_no_old_pid() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path();
        std::fs::create_dir_all(bt.join("pids")).unwrap();
        assert!(migrate_old_pid_file(bt).is_ok());
    }
}
