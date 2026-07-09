use anyhow::Result;
use chrono::{Local, TimeZone, Utc};

use crate::db::Database;
use crate::models::Session;
use crate::{BOLD, DIM, GREEN, RESET, find_project, groups};

pub fn cmd_session_start(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("session name cannot be empty");
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let session = db.start_session(project.id, name, "manual")?;

    println!(
        "{}Session started{} {} at {}",
        GREEN,
        RESET,
        session.name,
        format_local_time(session.started_at)
    );
    Ok(())
}

pub fn cmd_session_stop() -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;

    let Some(session) = db.stop_active_session(project.id)? else {
        println!("No active session.");
        return Ok(());
    };
    let event_count = db.get_session_events(&session)?.len();
    println!(
        "{}Session stopped{} {} ({} event(s))",
        GREEN, RESET, session.name, event_count
    );
    Ok(())
}

pub fn cmd_sessions() -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let sessions = db.list_sessions(project.id)?;

    if sessions.is_empty() {
        println!("No sessions yet.");
        return Ok(());
    }

    println!("{}undo{} — sessions", BOLD, RESET);
    println!();
    for session in sessions {
        print_session_line(&session);
    }
    Ok(())
}

pub fn cmd_session_show(name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let session = db
        .get_session_by_name(project.id, name)?
        .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name))?;
    let events = db.get_session_events(&session)?;
    let groups = groups::build_groups(&project, &events);

    println!("{}undo{} — session {}", BOLD, RESET, session.name);
    println!("Started: {}", format_local_time(session.started_at));
    match session.ended_at {
        Some(ended_at) => println!("Ended:   {}", format_local_time(ended_at)),
        None => println!("Ended:   active"),
    }
    println!("Events:  {}", events.len());

    if groups.is_empty() {
        println!();
        println!("No change groups yet.");
        return Ok(());
    }

    println!();
    println!("{}Change groups{}", BOLD, RESET);
    for group in groups {
        println!(
            "  {}{}{} {} - {} file(s), {} event(s), +{} -{}",
            DIM,
            group.id,
            RESET,
            group.label,
            group.paths.len(),
            group.event_count,
            group.inserted,
            group.deleted
        );
    }
    Ok(())
}

fn print_session_line(session: &Session) {
    let status = if session.ended_at.is_some() {
        "stopped"
    } else {
        "active"
    };
    let elapsed = Utc::now().timestamp().saturating_sub(session.started_at);
    println!(
        "{}{}{} {} ({}, {})",
        DIM,
        format_local_time(session.started_at),
        RESET,
        session.name,
        status,
        crate::duration::format_elapsed(elapsed)
    );
}

fn format_local_time(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string())
}
