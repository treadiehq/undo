use anyhow::Result;
use chrono::{Local, TimeZone, Utc};
use std::collections::BTreeSet;

use crate::db::Database;
use crate::duration;
use crate::models::{Checkpoint, FileEvent, WatchedProject};
use crate::{BLUE, BOLD, DIM, GREEN, RED, RESET, YELLOW, find_project, relative_path};

const BURST_GAP_SECS: i64 = 10;
const BURST_MIN_EVENTS: usize = 8;
const BURST_MIN_PATHS: usize = 5;
const BURST_MIN_DELETES: usize = 2;
const PANIC_WINDOW_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone)]
struct Burst {
    start: i64,
    end: i64,
    event_count: usize,
    path_count: usize,
    deleted_count: usize,
}

pub fn cmd_checkpoint(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("checkpoint name cannot be empty");
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let now = Utc::now().timestamp();
    db.create_checkpoint(project.id, name, now)?;

    println!("{}Checkpoint saved{} {}", GREEN, RESET, name);
    Ok(())
}

pub fn cmd_checkpoints() -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let checkpoints = db.list_checkpoints(project.id)?;

    if checkpoints.is_empty() {
        println!("No checkpoints yet.");
        return Ok(());
    }

    println!("{}undo{} — checkpoints", BOLD, RESET);
    println!();
    for checkpoint in checkpoints {
        let age = duration::format_elapsed(Utc::now().timestamp() - checkpoint.timestamp);
        println!("{}{}{} {}", DIM, age, RESET, checkpoint.name);
    }

    Ok(())
}

pub fn cmd_timeline(
    limit: usize,
    since: Option<&str>,
    show_bursts: bool,
    deleted_only: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let since_ts = parse_since(since)?;
    let mut events = match since_ts {
        Some(ts) => db.get_events_since(project.id, ts)?,
        None => db.get_timeline(project.id, limit)?,
    };

    if deleted_only {
        events.retain(|e| e.event_type == "DELETED");
    }

    if events.is_empty() {
        println!("No events recorded yet.");
        return Ok(());
    }

    let bursts = detect_bursts(&events);

    println!("{}undo{} — recent activity", BOLD, RESET);
    if show_bursts {
        print_bursts(&bursts);
    }
    println!();

    for event in &events {
        print_event(&project, event);
    }

    if !deleted_only {
        let checkpoints = filtered_checkpoints(&db, project.id, since_ts)?;
        if !checkpoints.is_empty() {
            println!();
            println!("{}Checkpoints{}", BOLD, RESET);
            for checkpoint in checkpoints.iter().take(5) {
                print_checkpoint(checkpoint);
            }
        }
    }

    Ok(())
}

pub fn cmd_deleted(limit: usize) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let events = db.get_deleted_events(project.id, limit)?;

    if events.is_empty() {
        println!("No recoverable deleted files found.");
        return Ok(());
    }

    println!("{}undo{} — recoverable deleted files", BOLD, RESET);
    println!();
    for event in events {
        let age = duration::format_elapsed(Utc::now().timestamp() - event.timestamp);
        let rel = relative_path(&event.path, &project.root_path);
        println!("{}{}{} {}", DIM, age, RESET, rel);
    }

    Ok(())
}

pub fn cmd_panic(restore_before_latest_burst: bool, yes: bool) -> Result<()> {
    if restore_before_latest_burst && !yes {
        anyhow::bail!("panic restore requires --yes");
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let since = Utc::now().timestamp().saturating_sub(PANIC_WINDOW_SECS);
    let events = db.get_events_since(project.id, since)?;
    let bursts = detect_bursts(&events);

    if restore_before_latest_burst {
        let Some(latest) = bursts.iter().max_by_key(|b| b.end) else {
            anyhow::bail!("no recent change burst found to restore before");
        };
        let target = latest.start.saturating_sub(1);
        return crate::restore::restore_at_timestamp(
            ".",
            target,
            "before latest burst",
            false,
            yes,
        );
    }

    println!("{}undo panic{} — recovery dashboard", BOLD, RESET);
    println!();

    if let Some(latest) = bursts.iter().max_by_key(|b| b.end) {
        println!(
            "{}Latest burst{}: {} files, {} events, {} deleted around {}",
            YELLOW,
            RESET,
            latest.path_count,
            latest.event_count,
            latest.deleted_count,
            format_local_time(latest.end)
        );
        let age_secs = Utc::now()
            .timestamp()
            .saturating_sub(latest.start.saturating_sub(1));
        println!("  Preview: undo preview . {}s", age_secs);
        println!("  Restore: undo panic --restore-before-latest-burst --yes");
    } else {
        println!("No large recent change bursts found.");
    }

    let deleted = db.get_deleted_events(project.id, 5)?;
    if !deleted.is_empty() {
        println!();
        println!("{}Recently deleted{}", BOLD, RESET);
        for event in deleted {
            let rel = relative_path(&event.path, &project.root_path);
            let age = duration::format_elapsed(Utc::now().timestamp() - event.timestamp);
            println!("  {}{}{} {}", DIM, age, RESET, rel);
        }
    }

    let checkpoints = db.list_checkpoints(project.id)?;
    if !checkpoints.is_empty() {
        println!();
        println!("{}Recent checkpoints{}", BOLD, RESET);
        for checkpoint in checkpoints.iter().take(5) {
            println!(
                "  {}{}{} restore with: undo restore --checkpoint {:?} . --yes",
                DIM,
                duration::format_elapsed(Utc::now().timestamp() - checkpoint.timestamp),
                RESET,
                checkpoint.name
            );
        }
    }

    println!();
    println!("Recommended first step: run a preview command before restoring.");
    Ok(())
}

fn parse_since(since: Option<&str>) -> Result<Option<i64>> {
    since
        .map(|s| {
            let secs = duration::parse_duration(s)?;
            Ok(Utc::now().timestamp().saturating_sub(secs))
        })
        .transpose()
}

fn filtered_checkpoints(
    db: &Database,
    project_id: i64,
    since: Option<i64>,
) -> Result<Vec<Checkpoint>> {
    let mut checkpoints = db.list_checkpoints(project_id)?;
    if let Some(since) = since {
        checkpoints.retain(|c| c.timestamp >= since);
    }
    Ok(checkpoints)
}

fn detect_bursts(events: &[FileEvent]) -> Vec<Burst> {
    let mut sorted = events.to_vec();
    sorted.sort_by_key(|e| (e.timestamp, e.id));

    let mut bursts = Vec::new();
    let mut group: Vec<FileEvent> = Vec::new();

    for event in sorted {
        let starts_new_group = group
            .last()
            .is_some_and(|prev| event.timestamp - prev.timestamp > BURST_GAP_SECS);
        if starts_new_group {
            maybe_push_burst(&mut bursts, &group);
            group.clear();
        }
        group.push(event);
    }
    maybe_push_burst(&mut bursts, &group);

    bursts
}

fn maybe_push_burst(bursts: &mut Vec<Burst>, group: &[FileEvent]) {
    if group.is_empty() {
        return;
    }

    let paths = group
        .iter()
        .map(|e| e.path.as_str())
        .collect::<BTreeSet<_>>();
    let deleted_count = group.iter().filter(|e| e.event_type == "DELETED").count();

    if group.len() >= BURST_MIN_EVENTS
        || paths.len() >= BURST_MIN_PATHS
        || deleted_count >= BURST_MIN_DELETES
    {
        bursts.push(Burst {
            start: group.first().map(|e| e.timestamp).unwrap_or_default(),
            end: group.last().map(|e| e.timestamp).unwrap_or_default(),
            event_count: group.len(),
            path_count: paths.len(),
            deleted_count,
        });
    }
}

fn print_bursts(bursts: &[Burst]) {
    println!();
    if bursts.is_empty() {
        println!("No large change bursts found.");
        return;
    }

    println!("{}Change bursts{}", BOLD, RESET);
    for burst in bursts.iter().rev() {
        println!(
            "  {}{}{} {} files, {} events, {} deleted",
            DIM,
            format_local_time(burst.end),
            RESET,
            burst.path_count,
            burst.event_count,
            burst.deleted_count
        );
    }
}

fn print_event(project: &WatchedProject, event: &FileEvent) {
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

fn print_checkpoint(checkpoint: &Checkpoint) {
    println!(
        "{}{}{} {}{}{}",
        DIM,
        format_local_time(checkpoint.timestamp),
        RESET,
        BLUE,
        checkpoint.name,
        RESET
    );
}

fn format_local_time(timestamp: i64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, timestamp: i64, path: &str, event_type: &str) -> FileEvent {
        FileEvent {
            id,
            project_id: 1,
            timestamp,
            path: path.to_string(),
            event_type: event_type.to_string(),
            current_hash: None,
            previous_hash: None,
            snapshot_path: None,
            old_path: None,
            file_size: None,
        }
    }

    #[test]
    fn detect_bursts_groups_rapid_many_file_changes() {
        let events = vec![
            event(1, 100, "/p/a.rs", "MODIFIED"),
            event(2, 101, "/p/b.rs", "MODIFIED"),
            event(3, 102, "/p/c.rs", "MODIFIED"),
            event(4, 103, "/p/d.rs", "MODIFIED"),
            event(5, 104, "/p/e.rs", "MODIFIED"),
        ];

        let bursts = detect_bursts(&events);
        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].path_count, 5);
        assert_eq!(bursts[0].event_count, 5);
    }

    #[test]
    fn detect_bursts_splits_on_time_gap() {
        let events = vec![
            event(1, 100, "/p/a.rs", "MODIFIED"),
            event(2, 120, "/p/b.rs", "MODIFIED"),
            event(3, 121, "/p/c.rs", "MODIFIED"),
            event(4, 122, "/p/d.rs", "MODIFIED"),
            event(5, 123, "/p/e.rs", "MODIFIED"),
            event(6, 124, "/p/f.rs", "MODIFIED"),
        ];

        let bursts = detect_bursts(&events);
        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].start, 120);
    }
}
