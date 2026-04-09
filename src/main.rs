use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::path::Path;

mod cli;
mod daemon;
mod db;
mod diff;
mod duration;
mod ignore;
mod models;
mod restore;
mod snapshots;
mod update;
mod watcher;

// ── ANSI colors ─────────────────────────────────────────────────────

pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RESET: &str = "\x1b[0m";

// ── helpers ─────────────────────────────────────────────────────────

pub fn backtrack_dir() -> Result<std::path::PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".undo");
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(dir.join("snapshots"))?;
    Ok(dir)
}

pub fn find_project(db: &db::Database, cwd: &Path) -> Result<models::WatchedProject> {
    db.find_project_for_path(cwd)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no project is being watched for this directory.\nRun `undo start` first."
        )
    })
}

pub fn relative_path<'a>(abs_path: &'a str, project_root: &str) -> &'a str {
    abs_path
        .strip_prefix(project_root)
        .and_then(|p| p.strip_prefix('/'))
        .unwrap_or(abs_path)
}

fn format_local_time(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "??:??".to_string())
}

fn event_color(event_type: &str) -> &'static str {
    match event_type {
        "MODIFIED" => YELLOW,
        "CREATED" => GREEN,
        "DELETED" => RED,
        "RENAMED" => BLUE,
        _ => "",
    }
}

// ── entry point ─────────────────────────────────────────────────────

fn main() {
    let cli = cli::Cli::parse();

    let result = match cli.command {
        cli::Command::Start => daemon::cmd_start(cli.verbose),
        cli::Command::Timeline { limit } => cmd_timeline(limit),
        cli::Command::WhatChanged { duration } => cmd_what_changed(&duration),
        cli::Command::Diff { path } => diff::cmd_diff(&path),
        cli::Command::Restore { path, duration } => restore::cmd_restore(&path, &duration),
        cli::Command::Status => daemon::cmd_status(),
        cli::Command::Stop => daemon::cmd_stop(),
        cli::Command::Update => update::cmd_update(),
    };

    if let Err(e) = result {
        eprintln!("{}error:{} {}", RED, RESET, e);
        std::process::exit(1);
    }
}

// ── timeline ────────────────────────────────────────────────────────

fn cmd_timeline(limit: usize) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = db::Database::open()?;
    let project = find_project(&db, &cwd)?;

    let events = db.get_timeline(project.id, limit)?;

    if events.is_empty() {
        println!("No events recorded yet.");
        return Ok(());
    }

    println!("{}undo{} — recent activity", BOLD, RESET);
    println!();

    for event in &events {
        let time = format_local_time(event.timestamp);
        let color = event_color(&event.event_type);
        let rel = relative_path(&event.path, &project.root_path);

        if event.event_type == "RENAMED" {
            let old = event.old_path.as_deref().unwrap_or("?");
            let old_rel = relative_path(old, &project.root_path);
            println!(
                "{}{}{} {}{}{} {} -> {}",
                DIM, time, RESET, color, event.event_type, RESET, old_rel, rel
            );
        } else {
            println!(
                "{}{}{} {}{}{} {}",
                DIM, time, RESET, color, event.event_type, RESET, rel
            );
        }
    }

    Ok(())
}

// ── what-changed ────────────────────────────────────────────────────

fn cmd_what_changed(duration_str: &str) -> Result<()> {
    let secs = duration::parse_duration(duration_str)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = db::Database::open()?;
    let project = find_project(&db, &cwd)?;

    let since = chrono::Utc::now().timestamp() - secs;
    let events = db.get_events_since(project.id, since)?;

    if events.is_empty() {
        println!("No changes in the last {}.", duration_str);
        return Ok(());
    }

    // Keep only the most recent event type per path (events arrive newest-first).
    let mut latest: HashMap<String, String> = HashMap::new();
    for event in &events {
        latest
            .entry(event.path.clone())
            .or_insert_with(|| event.event_type.clone());
    }

    // Group paths by event type.
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (path, etype) in &latest {
        grouped.entry(etype.clone()).or_default().push(path.clone());
    }

    println!("{}Changes in last {}{}", BOLD, duration_str, RESET);
    println!();

    for etype in &["MODIFIED", "CREATED", "DELETED", "RENAMED"] {
        if let Some(paths) = grouped.get(*etype) {
            let color = event_color(etype);
            println!("{}{}{}", color, etype, RESET);
            let mut sorted = paths.clone();
            sorted.sort();
            for path in &sorted {
                println!("  - {}", relative_path(path, &project.root_path));
            }
            println!();
        }
    }

    Ok(())
}
