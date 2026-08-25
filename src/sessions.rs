use anyhow::Result;
use chrono::{Local, TimeZone};

use crate::db::Database;
use crate::{BOLD, DIM, GREEN, RESET, groups, resolve_project};

pub fn cmd_session_start(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("Run name cannot be empty");
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let session = db.start_session(project.id, name, "manual")?;

    println!(
        "{}Recording started for Run {}{} ({}) at {}.",
        GREEN,
        session.public_id(),
        RESET,
        session.name,
        format_local_time(session.started_at)
    );
    Ok(())
}

pub fn cmd_session_stop() -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;

    let Some(session) = db.stop_active_session(project.id)? else {
        println!("No active Run.");
        return Ok(());
    };
    let event_count = db.get_session_events(&session)?.len();
    let change_word = if event_count == 1 {
        "file change"
    } else {
        "file changes"
    };
    println!(
        "{}Run completed{} {} ({}, {} {}).",
        GREEN,
        RESET,
        session.public_id(),
        session.name,
        event_count,
        change_word
    );
    Ok(())
}

pub fn cmd_session_show(name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let session = db
        .get_session_by_name(project.id, name)?
        .ok_or_else(|| anyhow::anyhow!("Run '{}' not found", name))?;
    let events = db.get_session_events(&session)?;
    let groups = groups::build_groups(&project, &events);

    println!(
        "{}Run {}{} — {}",
        BOLD,
        session.public_id(),
        RESET,
        session.name
    );
    println!("Started: {}", format_local_time(session.started_at));
    if let Some(ended_at) = session.ended_at {
        println!("Finished: {}", format_local_time(ended_at));
    }
    println!("File changes: {}", events.len());

    if groups.is_empty() {
        println!();
        println!("No groups of file changes yet.");
        return Ok(());
    }

    println!();
    println!("{}File groups{}", BOLD, RESET);
    for group in groups {
        println!(
            "  {} — {} files, {} recorded changes, +{} -{} {}({}){}",
            group.label,
            group.paths.len(),
            group.event_count,
            group.inserted,
            group.deleted,
            DIM,
            group.id,
            RESET
        );
    }
    Ok(())
}

fn format_local_time(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string())
}
