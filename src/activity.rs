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

pub fn cmd_checkpoint_for(
    name: &str,
    intent: Option<&str>,
    run_reference: Option<&str>,
    json: bool,
) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("A checkpoint needs a name.");
    }

    let (db, project, _) = crate::runs::prepare_project_boundary()?;
    let run_id = match run_reference {
        Some(reference) => Some(
            db.get_run_by_ref(project.id, reference)?
                .ok_or_else(|| anyhow::anyhow!("Run '{}' not found", reference))?
                .id,
        ),
        None => db.get_active_session(project.id)?.map(|run| run.id),
    };
    let (checkpoint, created) = db.create_checkpoint_now(project.id, run_id, name, intent)?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "checkpoint": checkpoint,
                "checkpoint_id": checkpoint.public_id(),
                "created": created,
            }))?
        );
    } else if created {
        println!(
            "{}Checkpoint saved:{} {} ({})",
            GREEN,
            RESET,
            name,
            checkpoint.public_id()
        );
    } else {
        println!(
            "Checkpoint {} already exists at this point; no changes made.",
            checkpoint.public_id()
        );
    }
    Ok(())
}

pub fn cmd_checkpoints() -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let checkpoints = db.list_checkpoints(project.id)?;

    if checkpoints.is_empty() {
        println!("No checkpoints yet.");
        println!("Create one with: undo checkpoint <name>");
        return Ok(());
    }

    println!("{}Checkpoints{}", BOLD, RESET);
    println!();
    for checkpoint in checkpoints {
        let age = duration::format_elapsed(Utc::now().timestamp() - checkpoint.timestamp);
        println!(
            "{:<8} {}{}{} {}{}{}",
            checkpoint.public_id(),
            DIM,
            age,
            RESET,
            checkpoint.name,
            checkpoint
                .run_id
                .map(|run_id| format!(" (Run r_{})", run_id))
                .unwrap_or_default(),
            checkpoint
                .event_id
                .map(|event_id| format!(" (change {})", event_id))
                .unwrap_or_else(|| " (saved by time)".to_string())
        );
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
        Some(ts) => db.get_events_since_limited(project.id, ts, limit)?,
        None => db.get_timeline(project.id, limit)?,
    };

    if deleted_only {
        events.retain(|e| e.event_type == "DELETED");
    }

    if events.is_empty() {
        println!("No file changes recorded yet.");
        return Ok(());
    }

    let bursts = detect_bursts(&events);

    println!("{}Recent file changes{}", BOLD, RESET);
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

    println!("{}Recoverable deleted files{}", BOLD, RESET);
    println!();
    for event in events {
        let age = duration::format_elapsed(Utc::now().timestamp() - event.timestamp);
        let rel = relative_path(&event.path, &project.root_path);
        println!("{}{}{} {}", DIM, age, RESET, rel);
    }
    println!();
    println!("Restore one with: undo restore-deleted <path>");

    Ok(())
}

pub fn cmd_panic(restore_before_latest_burst: bool, yes: bool) -> Result<()> {
    if restore_before_latest_burst && !yes {
        anyhow::bail!(
            "No files changed: the emergency restore requires --yes.\nRerun with --yes to give Undo permission to change files."
        );
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let since = Utc::now().timestamp().saturating_sub(PANIC_WINDOW_SECS);
    let events = db.get_events_since(project.id, since)?;
    let bursts = detect_bursts(&events);

    if restore_before_latest_burst {
        let Some(latest) = bursts.iter().max_by_key(|b| b.end) else {
            anyhow::bail!("No recent group of rapid file changes was found.");
        };
        let target = latest.start.saturating_sub(1);
        let recovery = crate::recoveries::create_timestamp_recovery(
            ".",
            target,
            "Emergency recovery to before the latest destructive burst",
            "panic",
        )?;
        return crate::recoveries::cmd_apply(&recovery.public_id());
    }

    println!("{}Emergency recovery options{}", BOLD, RESET);
    println!();

    if let Some(latest) = bursts.iter().max_by_key(|b| b.end) {
        println!(
            "{}Latest rapid change group{}: {} files, {} changes, {} deleted around {}",
            YELLOW,
            RESET,
            latest.path_count,
            latest.event_count,
            latest.deleted_count,
            format_local_time(latest.end)
        );
        let target = latest.start.saturating_sub(1);
        let (preview_command, restore_command) = panic_restore_commands(target);
        println!("  Target timestamp: {}", target);
        println!("  Preview: {}", preview_command);
        println!("  Restore: {}", restore_command);
    } else {
        println!("No large group of recent file changes was found.");
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
        println!("  Restore one with: undo restore-deleted <path>");
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
    println!(
        "Emergency recovery uses timing, not task ownership. Prefer a Run or checkpoint when available."
    );
    println!("Next: run the Preview command before using Restore.");
    Ok(())
}

fn panic_restore_commands(target: i64) -> (String, String) {
    (
        format!("undo restore . --timestamp {} --preview", target),
        format!("undo restore . --timestamp {} --yes", target),
    )
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
        println!("No large groups of rapid file changes found.");
        return;
    }

    println!("{}Rapid file changes{}", BOLD, RESET);
    for burst in bursts.iter().rev() {
        println!(
            "  {}{}{} {} files, {} changes, {} deleted",
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
            DIM,
            time,
            RESET,
            color,
            event_label(&event.event_type),
            RESET,
            old_rel,
            rel
        );
    } else {
        println!(
            "{}{}{} {}{}{} {}",
            DIM,
            time,
            RESET,
            color,
            event_label(&event.event_type),
            RESET,
            rel
        );
    }
}

fn event_label(event_type: &str) -> &str {
    match event_type {
        "MODIFIED" => "Modified",
        "CREATED" => "Created",
        "DELETED" => "Deleted",
        "RENAMED" => "Renamed",
        other => other,
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

    #[test]
    fn panic_commands_share_the_exact_restore_target() {
        let (preview, restore) = panic_restore_commands(999);

        assert_eq!(preview, "undo restore . --timestamp 999 --preview");
        assert_eq!(restore, "undo restore . --timestamp 999 --yes");
    }

    #[test]
    fn event_labels_are_plain_language_without_changing_event_values() {
        assert_eq!(event_label("MODIFIED"), "Modified");
        assert_eq!(event_label("CREATED"), "Created");
        assert_eq!(event_label("DELETED"), "Deleted");
        assert_eq!(event_label("RENAMED"), "Renamed");
        assert_eq!(event_label("CUSTOM"), "CUSTOM");
    }
}
