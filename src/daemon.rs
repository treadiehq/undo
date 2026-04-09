use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::db::Database;
use crate::watcher;
use crate::{backtrack_dir, BOLD, GREEN, RED, RESET, YELLOW};

pub fn cmd_start(verbose: bool) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let bt_dir = backtrack_dir()?;
    let pid_path = bt_dir.join("pid");

    // Refuse to start if daemon is already running.
    if pid_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&pid_path) {
            if let Some(pid) = contents.lines().next().and_then(|s| s.parse::<u32>().ok()) {
                if is_pid_running(pid) {
                    let project = contents.lines().nth(1).unwrap_or("unknown");
                    println!("Backtrack is already running (PID {}).", pid);
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

    println!("{}Backtrack{} — filesystem history", BOLD, RESET);
    println!("Watching: {}", cwd.display());
    println!("Recording changes...");
    println!();

    watcher::initial_scan(&db, &project, &cwd, verbose)?;
    watcher::watch_directory(&db, &project, &cwd, shutdown, verbose)?;

    let _ = std::fs::remove_file(&pid_path);
    eprintln!("\nBacktrack stopped.");

    Ok(())
}

pub fn cmd_stop() -> Result<()> {
    let bt_dir = backtrack_dir()?;
    let pid_path = bt_dir.join("pid");

    if !pid_path.exists() {
        println!("No Backtrack daemon is running.");
        return Ok(());
    }

    let contents = std::fs::read_to_string(&pid_path)?;
    let pid: u32 = contents
        .lines()
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid PID file"))?;

    if !is_pid_running(pid) {
        println!("Daemon is not running (stale PID file). Cleaning up.");
        std::fs::remove_file(&pid_path)?;
        return Ok(());
    }

    std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;

    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = std::fs::remove_file(&pid_path);

    println!("Backtrack daemon stopped (PID {}).", pid);
    Ok(())
}

pub fn cmd_status() -> Result<()> {
    let bt_dir = backtrack_dir()?;
    let db = Database::open()?;
    let cwd = std::env::current_dir()?.canonicalize()?;

    println!("{}Backtrack{} — status", BOLD, RESET);
    println!();

    match db.find_project_for_path(&cwd)? {
        Some(project) => {
            println!("Project:   {}", project.root_path);

            let pid_path = bt_dir.join("pid");
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
                "Run {}backtrack start{} to begin watching.",
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
