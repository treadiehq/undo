//! Read-side payload builders for the local web UI (`undo ui`).
//!
//! Every function here takes an explicit [`Database`] and [`WatchedProject`]
//! and returns plain serializable data. The HTTP layer in `webui.rs` stays a
//! thin shell, and everything in this module is unit-testable without a
//! server, a daemon, or a real `~/.undo`.

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use similar::{ChangeTag, TextDiff};
use std::collections::HashSet;
use std::path::Path;

use crate::db::Database;
use crate::models::{Checkpoint, FileEvent, Recovery, Session, WatchedProject};
use crate::{relative_path, snapshots};

/// Idle gap that separates two groups of un-attributed file changes. Human
/// editing pauses are much longer than watcher debounce bursts, so this is
/// deliberately coarser than the 10-second burst window in `activity.rs`.
const EDIT_GROUP_GAP_SECS: i64 = 5 * 60;

/// Per-item cap on files that get line-level diff statistics. Snapshot pairs
/// are decompressed to compute net +/- counts; an enormous run should not
/// stall the timeline endpoint.
const MAX_FILES_WITH_STATS: usize = 500;

// ── projects ────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ProjectSummary {
    pub id: i64,
    pub root_path: String,
    pub name: String,
    pub recording: bool,
    pub event_count: i64,
    pub first_event_at: Option<i64>,
    pub last_event_at: Option<i64>,
}

pub fn project_summaries(db: &Database) -> Result<Vec<ProjectSummary>> {
    let mut summaries = Vec::new();
    for project_id in db.get_all_project_ids()? {
        let Some(project) = project_by_id(db, project_id)? else {
            continue;
        };
        summaries.push(project_summary(db, &project)?);
    }
    // Most recently active first; idle projects fall to the bottom.
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_event_at.unwrap_or(0)));
    Ok(summaries)
}

pub fn project_summary(db: &Database, project: &WatchedProject) -> Result<ProjectSummary> {
    let (first_event_at, last_event_at) = db.event_time_bounds(project.id)?;
    Ok(ProjectSummary {
        id: project.id,
        name: project_name(&project.root_path),
        recording: crate::daemon::is_recording(Path::new(&project.root_path)).unwrap_or(false),
        event_count: db.count_events(project.id)?,
        first_event_at,
        last_event_at,
        root_path: project.root_path.clone(),
    })
}

pub fn project_by_id(db: &Database, project_id: i64) -> Result<Option<WatchedProject>> {
    db.get_project_by_id(project_id)
}

fn project_name(root_path: &str) -> String {
    root_path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(root_path)
        .to_string()
}

// ── timeline ────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FileChange {
    /// Project-relative path.
    pub path: String,
    /// Net effect over the item: created | modified | deleted | renamed.
    pub change: String,
    pub event_count: usize,
    pub first_event_id: i64,
    pub last_event_id: i64,
    pub last_timestamp: i64,
    pub inserted: usize,
    pub deleted: usize,
    pub binary: bool,
    pub old_path: Option<String>,
    /// Integration-claim classification, not forensic process provenance.
    pub ownership_status: String,
    pub recoverable: bool,
    pub warning: Option<String>,
}

#[derive(Serialize)]
pub struct TimelineItem {
    /// `r_<id>` for Runs, `g_<first change id>` for un-attributed groups.
    pub id: String,
    /// "run", "collision", or "edits".
    pub kind: String,
    /// Display label: agent name or "Unattributed changes".
    pub label: String,
    pub actor: String,
    pub agent: Option<String>,
    pub command: Option<String>,
    pub intent: Option<String>,
    /// Run status, or "recorded" for un-attributed groups.
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub run_id: Option<String>,
    /// Editing rhythm: "machine" for tool-speed bursts, "human" for
    /// hand-paced changes, "run" for attributed Runs. Undo cannot name the
    /// process behind un-attributed changes, but it can honestly report how
    /// they arrived.
    pub pace: String,
    /// Dominant directory when most files share one, e.g. "src/auth".
    pub scope_hint: Option<String>,
    /// Restore boundary: state as of this change id is "before this item".
    pub boundary_event_id: i64,
    pub last_event_id: i64,
    pub event_count: usize,
    pub file_count: usize,
    pub inserted: usize,
    pub deleted: usize,
    /// Files whose net effect is deletion — surfaced so the UI can flag
    /// destructive bursts.
    pub deleted_files: usize,
    pub stats_truncated: bool,
    pub files: Vec<FileChange>,
    pub checkpoints: Vec<Checkpoint>,
}

/// Emergency signal for the UI: the newest un-attributed group inside the
/// panic window that deleted multiple files. Mirrors the spirit of
/// `undo panic` — timing-based, preview-first.
#[derive(Serialize)]
pub struct PanicAlert {
    pub item_id: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub file_count: usize,
    pub deleted_files: usize,
    /// Restoring the project to this exact Unix timestamp lands just before
    /// the destructive group began.
    pub target_timestamp: i64,
}

const PANIC_WINDOW_SECS: i64 = 24 * 60 * 60;
const PANIC_MIN_DELETED_FILES: usize = 2;

#[derive(Serialize)]
pub struct TimelinePayload {
    pub project: ProjectSummary,
    pub items: Vec<TimelineItem>,
    pub checkpoints: Vec<Checkpoint>,
    pub alert: Option<PanicAlert>,
    pub max_event_id: i64,
    pub now: i64,
}

pub fn timeline_payload(
    db: &Database,
    project: &WatchedProject,
    limit: usize,
    since_secs: Option<i64>,
) -> Result<TimelinePayload> {
    let mut window = match since_secs {
        Some(secs) => {
            let since_ts = Utc::now().timestamp().saturating_sub(secs);
            db.get_events_since_limited(project.id, since_ts, limit)?
        }
        None => db.get_timeline(project.id, limit)?,
    };
    // Queries return newest-first; clustering wants chronological order.
    window.reverse();

    let window_start_id = window.iter().map(|event| event.id).min().unwrap_or(0);

    let mut items = Vec::new();
    let mut claimed: HashSet<i64> = HashSet::new();
    // A clock step can put a Run's events after its end timestamp. Select Runs
    // by event boundaries so those present in the visible history stay visible.
    let runs: Vec<_> = db
        .list_sessions(project.id)?
        .into_iter()
        .filter(|run| run.end_event_id.is_none_or(|id| id >= window_start_id))
        .collect();
    let max_event_id = db.max_event_id(project.id)?;
    let first_relevant_claim = runs
        .iter()
        .map(|run| run.start_event_id.saturating_add(1))
        .chain(std::iter::once(window_start_id))
        .min()
        .unwrap_or(window_start_id);
    let claim_counts = db.get_event_claim_counts(project.id, first_relevant_claim, max_event_id)?;
    for run in runs {
        let mut events = db.get_session_events(&run)?;
        // Net file changes and diff boundaries need commit order, not wall time.
        events.sort_by_key(|event| event.id);
        if run.is_reported() {
            events.retain(|event| claim_counts.get(&event.id).copied() == Some(1));
        }
        for event in &events {
            claimed.insert(event.id);
        }
        if events.is_empty() && !run.is_active() {
            continue;
        }
        items.push(run_item(project, &run, &events, db)?);
    }

    let collision_events = window
        .iter()
        .filter(|event| claim_counts.get(&event.id).copied().unwrap_or(0) >= 2)
        .cloned()
        .collect::<Vec<_>>();
    for group in split_by_gap(&collision_events, EDIT_GROUP_GAP_SECS) {
        items.push(collision_item(project, group, window_start_id));
    }

    let unclaimed: Vec<FileEvent> = window
        .into_iter()
        .filter(|event| {
            !claimed.contains(&event.id) && claim_counts.get(&event.id).copied().unwrap_or(0) == 0
        })
        .collect();
    for group in split_by_gap(&unclaimed, EDIT_GROUP_GAP_SECS) {
        items.push(edits_item(project, group, window_start_id));
    }

    items.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.last_event_id.cmp(&a.last_event_id))
    });

    let checkpoints = db.list_checkpoints(project.id)?;
    let now = Utc::now().timestamp();
    let resolved_after = db.latest_applied_recovery_at(project.id)?;
    Ok(TimelinePayload {
        project: project_summary(db, project)?,
        alert: panic_alert(&items, now, resolved_after),
        items,
        checkpoints,
        max_event_id,
        now,
    })
}

/// Surface the newest un-attributed group in the panic window that deleted
/// multiple files. Attributed Runs are excluded — they are reviewable and
/// undoable as a whole — and the deletion threshold keeps a recovery's own
/// rewrites from re-triggering the alarm. Bursts that predate the most
/// recently applied recovery are considered addressed and stay quiet.
fn panic_alert(
    items: &[TimelineItem],
    now: i64,
    resolved_after: Option<i64>,
) -> Option<PanicAlert> {
    items
        .iter()
        .filter(|item| {
            item.kind == "edits"
                && item.deleted_files >= PANIC_MIN_DELETED_FILES
                && item.started_at >= now.saturating_sub(PANIC_WINDOW_SECS)
                && resolved_after
                    .is_none_or(|resolved| item.ended_at.unwrap_or(item.started_at) > resolved)
        })
        .max_by_key(|item| item.started_at)
        .map(|item| PanicAlert {
            item_id: item.id.clone(),
            started_at: item.started_at,
            ended_at: item.ended_at.unwrap_or(item.started_at),
            file_count: item.file_count,
            deleted_files: item.deleted_files,
            target_timestamp: item.started_at.saturating_sub(1),
        })
}

fn run_item(
    project: &WatchedProject,
    run: &Session,
    events: &[FileEvent],
    db: &Database,
) -> Result<TimelineItem> {
    let (mut files, stats_truncated) = build_file_changes(project, events);
    if run.is_reported() {
        for file in &mut files {
            let absolute = Path::new(&project.root_path)
                .join(&file.path)
                .to_string_lossy()
                .into_owned();
            let status = db.classify_run_path_ownership(run.id, &absolute)?;
            file.recoverable = status == "exclusive";
            file.warning = ownership_warning(&status);
            file.ownership_status = status;
        }
    }
    let checkpoints = db
        .list_checkpoints(project.id)?
        .into_iter()
        .filter(|checkpoint| checkpoint.run_id == Some(run.id))
        .collect::<Vec<_>>();
    let label = run_label(run);
    Ok(TimelineItem {
        id: run.public_id(),
        kind: "run".to_string(),
        label,
        actor: run.actor.clone(),
        agent: run.agent.clone(),
        command: run.command.clone(),
        intent: run.intent.clone(),
        status: run.status.clone(),
        started_at: run.started_at,
        ended_at: run.ended_at,
        run_id: Some(run.public_id()),
        pace: "run".to_string(),
        scope_hint: scope_hint(&files),
        boundary_event_id: run.start_event_id,
        last_event_id: events.last().map(|event| event.id).unwrap_or(0),
        event_count: events.len(),
        file_count: files.len(),
        inserted: files.iter().map(|file| file.inserted).sum(),
        deleted: files.iter().map(|file| file.deleted).sum(),
        deleted_files: files.iter().filter(|file| file.change == "deleted").count(),
        stats_truncated,
        files,
        checkpoints,
    })
}

fn run_label(run: &Session) -> String {
    run.agent
        .clone()
        .unwrap_or_else(|| match run.actor.as_str() {
            "human" => "You".to_string(),
            "tool" => run.command.clone().unwrap_or_else(|| "Tool".to_string()),
            other => other.to_string(),
        })
}

fn edits_item(
    project: &WatchedProject,
    events: &[FileEvent],
    window_start_id: i64,
) -> TimelineItem {
    let (mut files, stats_truncated) = build_file_changes(project, events);
    for file in &mut files {
        file.ownership_status = "unattributed".to_string();
        file.warning = Some("No integration claimed this recorded change.".to_string());
    }
    let first = events.first().expect("gap groups are never empty");
    let last = events.last().expect("gap groups are never empty");
    // The restore boundary is "just before the first change in this group".
    // When the group starts exactly at the window edge, the state one id
    // earlier is still correct: resolvers scan `id <= boundary`.
    let boundary_event_id = first.id.saturating_sub(1).max(window_start_id - 1).max(0);
    let pace = classify_pace(events);
    TimelineItem {
        id: format!("g_{}", first.id),
        kind: "edits".to_string(),
        label: "Unattributed changes".to_string(),
        actor: "unattributed".to_string(),
        agent: None,
        command: None,
        intent: None,
        status: "recorded".to_string(),
        started_at: first.timestamp,
        ended_at: Some(last.timestamp),
        run_id: None,
        pace: pace.to_string(),
        scope_hint: scope_hint(&files),
        boundary_event_id,
        last_event_id: last.id,
        event_count: events.len(),
        file_count: files.len(),
        inserted: files.iter().map(|file| file.inserted).sum(),
        deleted: files.iter().map(|file| file.deleted).sum(),
        deleted_files: files.iter().filter(|file| file.change == "deleted").count(),
        stats_truncated,
        files,
        checkpoints: Vec::new(),
    }
}

fn collision_item(
    project: &WatchedProject,
    events: &[FileEvent],
    window_start_id: i64,
) -> TimelineItem {
    let (mut files, stats_truncated) = build_file_changes(project, events);
    for file in &mut files {
        file.ownership_status = "collision".to_string();
        file.recoverable = false;
        file.warning = Some(
            "Multiple Runs claimed this change; whole-file Run recovery is disabled.".to_string(),
        );
    }
    let first = events.first().expect("collision groups are never empty");
    let last = events.last().expect("collision groups are never empty");
    let boundary_event_id = first.id.saturating_sub(1).max(window_start_id - 1).max(0);
    TimelineItem {
        id: format!("collision_{}", first.id),
        kind: "collision".to_string(),
        label: "Attribution collision".to_string(),
        actor: "collision".to_string(),
        agent: None,
        command: None,
        intent: None,
        status: "blocked".to_string(),
        started_at: first.timestamp,
        ended_at: Some(last.timestamp),
        run_id: None,
        pace: "machine".to_string(),
        scope_hint: scope_hint(&files),
        boundary_event_id,
        last_event_id: last.id,
        event_count: events.len(),
        file_count: files.len(),
        inserted: files.iter().map(|file| file.inserted).sum(),
        deleted: files.iter().map(|file| file.deleted).sum(),
        deleted_files: files.iter().filter(|file| file.change == "deleted").count(),
        stats_truncated,
        files,
        checkpoints: Vec::new(),
    }
}

fn ownership_warning(status: &str) -> Option<String> {
    match status {
        "collision" => Some(
            "Multiple Runs claimed this file change; whole-file recovery is disabled.".to_string(),
        ),
        "interleaved" => Some(
            "Other or unattributed edits touched this path after the Run started; whole-file recovery is disabled."
                .to_string(),
        ),
        "unattributed" => Some(
            "This path has no explicit claim from the Run; whole-file recovery is disabled."
                .to_string(),
        ),
        _ => None,
    }
}

/// Distinguish tool-speed change groups from hand-paced editing. Six or more
/// changes averaging under two seconds apart cannot be typed by a person;
/// anything slower is reported as manual. This is a rhythm observation, not
/// process attribution — Undo does not pretend to know *which* tool ran.
fn classify_pace(events: &[FileEvent]) -> &'static str {
    let (Some(first), Some(last)) = (events.first(), events.last()) else {
        return "human";
    };
    let span = last.timestamp.saturating_sub(first.timestamp);
    if events.len() >= 6 && span <= events.len() as i64 * 2 {
        "machine"
    } else {
        "human"
    }
}

/// Dominant directory of an item's files (up to two leading components),
/// reported when at least 60% of the files live under it. Gives un-attributed
/// groups a scannable identity: "Unattributed changes · src/auth".
fn scope_hint(files: &[FileChange]) -> Option<String> {
    if files.len() < 2 {
        return None;
    }
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for file in files {
        if let Some(key) = leading_dirs(&file.path) {
            *counts.entry(key).or_default() += 1;
        }
    }
    let (key, count) = counts.into_iter().max_by_key(|(_, count)| *count)?;
    if count * 10 >= files.len() * 6 {
        Some(key)
    } else {
        None
    }
}

fn leading_dirs(rel_path: &str) -> Option<String> {
    let (dir, _) = rel_path.rsplit_once('/')?;
    let mut parts = dir.split('/').filter(|part| !part.is_empty());
    let first = parts.next()?;
    Some(match parts.next() {
        Some(second) => format!("{first}/{second}"),
        None => first.to_string(),
    })
}

fn split_by_gap(events: &[FileEvent], gap_secs: i64) -> Vec<&[FileEvent]> {
    let mut groups = Vec::new();
    let mut start = 0usize;
    for index in 1..events.len() {
        if events[index].timestamp - events[index - 1].timestamp > gap_secs {
            groups.push(&events[start..index]);
            start = index;
        }
    }
    if start < events.len() {
        groups.push(&events[start..]);
    }
    groups
}

/// Collapse chronological events into one net change per path.
fn build_file_changes(project: &WatchedProject, events: &[FileEvent]) -> (Vec<FileChange>, bool) {
    let mut order: Vec<&str> = Vec::new();
    let mut by_path: std::collections::HashMap<&str, Vec<&FileEvent>> =
        std::collections::HashMap::new();
    for event in events {
        let entry = by_path.entry(event.path.as_str()).or_default();
        if entry.is_empty() {
            order.push(event.path.as_str());
        }
        entry.push(event);
    }

    let stats_truncated = order.len() > MAX_FILES_WITH_STATS;
    let mut files = Vec::with_capacity(order.len());
    for (index, path) in order.iter().enumerate() {
        let path_events = &by_path[path];
        let first = path_events.first().expect("path groups are never empty");
        let last = path_events.last().expect("path groups are never empty");

        let change = net_change(first, last);
        let (inserted, deleted, binary) = if index < MAX_FILES_WITH_STATS {
            net_diff_stats(project.id, first, last)
        } else {
            (0, 0, false)
        };
        let old_path = path_events
            .iter()
            .rev()
            .find_map(|event| event.old_path.clone());

        files.push(FileChange {
            path: relative_path(path, &project.root_path).to_string(),
            change,
            event_count: path_events.len(),
            first_event_id: first.id,
            last_event_id: last.id,
            last_timestamp: last.timestamp,
            inserted,
            deleted,
            binary,
            old_path: old_path
                .as_deref()
                .map(|old| relative_path(old, &project.root_path).to_string()),
            ownership_status: "exclusive".to_string(),
            recoverable: true,
            warning: None,
        });
    }
    // Most heavily changed files first — that is what a reviewer scans for.
    files.sort_by_key(|file| std::cmp::Reverse(file.inserted + file.deleted));
    (files, stats_truncated)
}

fn net_change(first: &FileEvent, last: &FileEvent) -> String {
    if last.event_type == "DELETED" {
        "deleted".to_string()
    } else if first.event_type == "CREATED" {
        "created".to_string()
    } else if last.event_type == "RENAMED" {
        "renamed".to_string()
    } else {
        "modified".to_string()
    }
}

/// Net +/- line counts across an item: state before the first event diffed
/// against state after the last event. Missing or pruned snapshots degrade to
/// empty content instead of failing the listing.
fn net_diff_stats(project_id: i64, first: &FileEvent, last: &FileEvent) -> (usize, usize, bool) {
    let old = match first.event_type.as_str() {
        "CREATED" => Vec::new(),
        _ => first
            .previous_hash
            .as_deref()
            .and_then(|hash| snapshots::load(project_id, hash).ok())
            .unwrap_or_default(),
    };
    let new = match last.event_type.as_str() {
        "DELETED" => Vec::new(),
        _ => last
            .current_hash
            .as_deref()
            .and_then(|hash| snapshots::load(project_id, hash).ok())
            .unwrap_or_default(),
    };
    if crate::diff::is_binary(&old) || crate::diff::is_binary(&new) {
        return (0, 0, true);
    }
    if old == new {
        return (0, 0, false);
    }
    let (old_text, new_text) = crate::diff::render_bytes_for_diff(&old, &new);
    let diff = TextDiff::from_lines(old_text.as_ref(), new_text.as_ref());
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => inserted += 1,
            ChangeTag::Delete => deleted += 1,
            ChangeTag::Equal => {}
        }
    }
    (inserted, deleted, false)
}

// ── diffs ───────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DiffLine {
    /// "ctx" | "add" | "del"
    pub kind: String,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

#[derive(Serialize)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Serialize)]
pub struct DiffPayload {
    pub path: String,
    pub change: String,
    pub binary: bool,
    pub inserted: usize,
    pub deleted: usize,
    pub old_timestamp: Option<i64>,
    pub new_timestamp: Option<i64>,
    pub hunks: Vec<DiffHunk>,
}

/// Diff one file across an inclusive change-id range: state before the first
/// matching event against state after the last matching event.
pub fn diff_payload(
    db: &Database,
    project: &WatchedProject,
    rel_path: &str,
    first_event_id: i64,
    last_event_id: i64,
) -> Result<DiffPayload> {
    let abs_path = format!(
        "{}/{}",
        project.root_path.trim_end_matches('/'),
        rel_path.trim_start_matches('/')
    );
    let mut events =
        db.get_events_between_ids(project.id, first_event_id.saturating_sub(1), last_event_id)?;
    events.retain(|event| event.path == abs_path);
    events.sort_by_key(|event| event.id);
    let (Some(first), Some(last)) = (events.first(), events.last()) else {
        anyhow::bail!("No recorded changes for {} in this range.", rel_path);
    };

    let old = match first.event_type.as_str() {
        "CREATED" => Vec::new(),
        _ => first
            .previous_hash
            .as_deref()
            .and_then(|hash| snapshots::load(project.id, hash).ok())
            .unwrap_or_default(),
    };
    let new = match last.event_type.as_str() {
        "DELETED" => Vec::new(),
        _ => last
            .current_hash
            .as_deref()
            .and_then(|hash| snapshots::load(project.id, hash).ok())
            .unwrap_or_default(),
    };

    diff_from_contents(
        rel_path,
        &net_change(first, last),
        &old,
        &new,
        Some(first.timestamp),
        Some(last.timestamp),
    )
}

fn diff_from_contents(
    rel_path: &str,
    change: &str,
    old: &[u8],
    new: &[u8],
    old_timestamp: Option<i64>,
    new_timestamp: Option<i64>,
) -> Result<DiffPayload> {
    if crate::diff::is_binary(old) || crate::diff::is_binary(new) {
        return Ok(DiffPayload {
            path: rel_path.to_string(),
            change: change.to_string(),
            binary: true,
            inserted: 0,
            deleted: 0,
            old_timestamp,
            new_timestamp,
            hunks: Vec::new(),
        });
    }

    let (old_text, new_text) = crate::diff::render_bytes_for_diff(old, new);
    let text_diff = TextDiff::from_lines(old_text.as_ref(), new_text.as_ref());
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    let mut hunks = Vec::new();
    for group in text_diff.grouped_ops(3) {
        let (Some(first_op), Some(last_op)) = (group.first(), group.last()) else {
            continue;
        };
        let old_start = first_op.old_range().start;
        let old_len = last_op.old_range().end - old_start;
        let new_start = first_op.new_range().start;
        let new_len = last_op.new_range().end - new_start;
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => "ctx",
                    ChangeTag::Insert => {
                        inserted += 1;
                        "add"
                    }
                    ChangeTag::Delete => {
                        deleted += 1;
                        "del"
                    }
                };
                lines.push(DiffLine {
                    kind: kind.to_string(),
                    old_line: change.old_index().map(|index| index + 1),
                    new_line: change.new_index().map(|index| index + 1),
                    text: change.value().trim_end_matches('\n').to_string(),
                });
            }
        }
        hunks.push(DiffHunk {
            header: format!(
                "@@ -{},{} +{},{} @@",
                old_start + 1,
                old_len,
                new_start + 1,
                new_len
            ),
            lines,
        });
    }

    Ok(DiffPayload {
        path: rel_path.to_string(),
        change: change.to_string(),
        binary: false,
        inserted,
        deleted,
        old_timestamp,
        new_timestamp,
        hunks,
    })
}

/// A transient preview: calculating this never saves a recovery or writes files.
#[derive(Serialize)]
pub struct RestorePreviewPayload {
    pub timestamp: i64,
    pub entries: Vec<RecoveryEntryView>,
    pub diff: Option<DiffPayload>,
}

pub fn restore_preview_payload(
    db: &Database,
    project: &WatchedProject,
    timestamp: i64,
    rel_path: Option<&str>,
) -> Result<RestorePreviewPayload> {
    use crate::restore_fs::{CappedRead, ProjectPath, RestoreFs};

    let selected = rel_path
        .map(|path| ProjectPath::from_relative(Path::new(path)))
        .transpose()?;
    // Use the whole-project planner even for the selected file. Single-file
    // CLI recovery has different fallback semantics for absent/deleted paths.
    let plan = crate::restore::plan_restore(
        db,
        project,
        Path::new(&project.root_path),
        ".",
        timestamp,
        false,
    )?;
    let entries = crate::recoveries::prepare_restore_entries(project, &plan)?;
    let diff = if let Some(path) = selected {
        let display = path.display();
        let absolute = path.absolute(project);
        let entry = entries
            .iter()
            .find(|entry| Path::new(&entry.path) == absolute);
        let payload = if let Some(entry) = entry {
            let fs = RestoreFs::open(project)?;
            let (exists, current) = match fs.read_capped(&path, snapshots::MAX_SNAPSHOT_SIZE)? {
                CappedRead::Missing => (false, Vec::new()),
                CappedRead::Content(bytes) => (true, bytes),
                CappedRead::TooLarge => anyhow::bail!("{} is too large to preview", display),
            };
            let hash = exists.then(|| snapshots::hash_bytes(&current));
            if exists != entry.expected_exists || hash != entry.expected_hash {
                anyhow::bail!("{} changed while reading its preview. Try again.", display);
            }
            let restored = entry
                .target_hash
                .as_deref()
                .map(|hash| snapshots::load(project.id, hash))
                .transpose()?
                .unwrap_or_default();
            let change = if entry.action == "DELETE" {
                "deleted"
            } else if !exists {
                "created"
            } else {
                "modified"
            };
            diff_from_contents(&display, change, &current, &restored, None, Some(timestamp))?
        } else {
            diff_from_contents(&display, "unchanged", b"", b"", None, Some(timestamp))?
        };
        Some(payload)
    } else {
        None
    };
    Ok(RestorePreviewPayload {
        timestamp,
        entries: entries
            .iter()
            .map(|entry| RecoveryEntryView {
                path: relative_path(&entry.path, &project.root_path).to_string(),
                action: entry.action.clone(),
                source_timestamp: entry.source_timestamp,
            })
            .collect(),
        diff,
    })
}

// ── recoveries ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RecoveryEntryView {
    pub path: String,
    /// "WRITE" | "DELETE"
    pub action: String,
    pub source_timestamp: Option<i64>,
}

#[derive(Serialize)]
pub struct RecoveryView {
    pub id: String,
    pub status: String,
    pub kind: String,
    pub confidence: String,
    pub request: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub run_id: Option<String>,
    pub ambiguity: Option<String>,
    pub writes: usize,
    pub deletes: usize,
    pub entries: Vec<RecoveryEntryView>,
}

pub fn recovery_view(
    db: &Database,
    project: &WatchedProject,
    recovery: &Recovery,
) -> Result<RecoveryView> {
    let entries = db.get_recovery_entries(recovery.id)?;
    let views = entries
        .iter()
        .map(|entry| RecoveryEntryView {
            path: relative_path(&entry.path, &project.root_path).to_string(),
            action: entry.action.clone(),
            source_timestamp: entry.source_timestamp,
        })
        .collect::<Vec<_>>();
    Ok(RecoveryView {
        id: recovery.public_id(),
        status: recovery.status.clone(),
        kind: recovery.kind.clone(),
        confidence: recovery.confidence.clone(),
        request: recovery.request.clone(),
        created_at: recovery.created_at,
        expires_at: recovery.expires_at,
        run_id: recovery.run_id.map(|id| format!("r_{id}")),
        ambiguity: recovery.ambiguity.clone(),
        writes: views.iter().filter(|entry| entry.action == "WRITE").count(),
        deletes: views
            .iter()
            .filter(|entry| entry.action == "DELETE")
            .count(),
        entries: views,
    })
}

// ── polling ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ActiveRunSummary {
    /// Stable internal identity; the UI does not expose this as display copy.
    pub id: String,
    pub label: String,
    pub started_at: i64,
}

#[derive(Serialize)]
pub struct PollPayload {
    pub max_event_id: i64,
    pub recording: bool,
    pub active_run_id: Option<String>,
    pub active_runs: Vec<ActiveRunSummary>,
    pub now: i64,
}

pub fn poll_payload(db: &Database, project: &WatchedProject) -> Result<PollPayload> {
    let active_runs = db
        .list_active_runs(project.id)?
        .into_iter()
        .map(|run| ActiveRunSummary {
            id: run.public_id(),
            label: run_label(&run),
            started_at: run.started_at,
        })
        .collect::<Vec<_>>();
    Ok(PollPayload {
        max_event_id: db.max_event_id(project.id)?,
        recording: crate::daemon::is_recording(Path::new(&project.root_path)).unwrap_or(false),
        active_run_id: (active_runs.len() == 1).then(|| active_runs[0].id.clone()),
        active_runs,
        now: Utc::now().timestamp(),
    })
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

    fn project() -> WatchedProject {
        WatchedProject {
            id: 1,
            root_path: "/repo".to_string(),
            created_at: 0,
        }
    }

    /// Un-attributed changes separated by more than the idle gap become
    /// distinct timeline items; rapid sequences stay together.
    #[test]
    fn split_by_gap_groups_activity_sessions() {
        let events = vec![
            event(1, 1_000, "/repo/a.rs", "MODIFIED"),
            event(2, 1_030, "/repo/b.rs", "MODIFIED"),
            event(
                3,
                1_000 + 30 + EDIT_GROUP_GAP_SECS + 1,
                "/repo/c.rs",
                "MODIFIED",
            ),
        ];
        let groups = split_by_gap(&events, EDIT_GROUP_GAP_SECS);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 1);
    }

    /// A file created and repeatedly modified inside one item nets out as
    /// "created"; a file whose final event is a delete nets out as "deleted".
    #[test]
    fn net_change_collapses_event_chains() {
        let created = event(1, 100, "/repo/new.rs", "CREATED");
        let modified = event(2, 110, "/repo/new.rs", "MODIFIED");
        let deleted = event(3, 120, "/repo/new.rs", "DELETED");
        assert_eq!(net_change(&created, &modified), "created");
        assert_eq!(net_change(&modified, &deleted), "deleted");
        assert_eq!(net_change(&modified, &modified), "modified");
    }

    /// One net FileChange per path, ordered by churn, with correct
    /// first/last change ids for the diff endpoint.
    #[test]
    fn build_file_changes_collapses_paths() {
        let events = vec![
            event(1, 100, "/repo/src/a.rs", "CREATED"),
            event(2, 105, "/repo/src/a.rs", "MODIFIED"),
            event(3, 110, "/repo/src/b.rs", "MODIFIED"),
        ];
        let (files, truncated) = build_file_changes(&project(), &events);
        assert!(!truncated);
        assert_eq!(files.len(), 2);
        let a = files.iter().find(|file| file.path == "src/a.rs").unwrap();
        assert_eq!(a.change, "created");
        assert_eq!(a.event_count, 2);
        assert_eq!(a.first_event_id, 1);
        assert_eq!(a.last_event_id, 2);
    }

    /// Run diffs use commit order even when wall-clock timestamps run backwards.
    #[test]
    fn timeline_run_keeps_event_order_after_clock_steps() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(Path::new("/repo")).unwrap();
        let run = db
            .start_session(project.id, "clock-step", "manual")
            .unwrap();
        db.insert_event_at(project.id, "/repo/new.rs", "CREATED", run.started_at + 1000)
            .unwrap();
        let first_id = db.max_event_id(project.id).unwrap();
        db.insert_event_at(project.id, "/repo/new.rs", "MODIFIED", run.started_at - 5)
            .unwrap();
        let last_id = db.max_event_id(project.id).unwrap();
        db.complete_run(run.id, "completed").unwrap();

        let payload = timeline_payload(&db, &project, 100, None).unwrap();
        assert_eq!(payload.items.len(), 1);
        let item = &payload.items[0];
        assert_eq!(item.id, run.public_id());
        assert_eq!(item.event_count, 2);
        assert_eq!(item.last_event_id, last_id);
        assert_eq!(item.files.len(), 1);
        let file = &item.files[0];
        assert_eq!(file.change, "created");
        assert_eq!(file.first_event_id, first_id);
        assert_eq!(file.last_event_id, last_id);
        assert_eq!(file.last_timestamp, run.started_at - 5);
    }

    /// Future-dated events still belong to a completed Run in a limited
    /// timeline, while Runs that ended before the visible event range stay out.
    #[test]
    fn timeline_keeps_run_with_only_future_dated_events() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(Path::new("/repo")).unwrap();
        let old = db.start_session(project.id, "old", "manual").unwrap();
        db.insert_event_at(project.id, "/repo/old.rs", "CREATED", old.started_at)
            .unwrap();
        db.complete_run(old.id, "completed").unwrap();
        let run = db.start_session(project.id, "future", "manual").unwrap();
        db.insert_event_at(
            project.id,
            "/repo/future.rs",
            "CREATED",
            run.started_at + 1000,
        )
        .unwrap();
        db.complete_run(run.id, "completed").unwrap();

        let payload = timeline_payload(&db, &project, 1, None).unwrap();
        assert_eq!(payload.items.len(), 1);
        assert_eq!(payload.items[0].id, run.public_id());
        assert_eq!(payload.items[0].event_count, 1);
        assert_eq!(payload.items[0].files[0].path, "future.rs");
    }

    /// Six or more changes at tool speed read as "machine"; slow, sparse
    /// changes read as "human". Pace is rhythm, not attribution.
    #[test]
    fn pace_classification_uses_change_rhythm() {
        let rapid: Vec<FileEvent> = (0..8)
            .map(|index| {
                event(
                    index + 1,
                    100 + index,
                    &format!("/repo/f{index}.rs"),
                    "MODIFIED",
                )
            })
            .collect();
        assert_eq!(classify_pace(&rapid), "machine");

        let slow: Vec<FileEvent> = (0..6)
            .map(|index| {
                event(
                    index + 1,
                    100 + index * 60,
                    &format!("/repo/f{index}.rs"),
                    "MODIFIED",
                )
            })
            .collect();
        assert_eq!(classify_pace(&slow), "human");

        let sparse = vec![event(1, 100, "/repo/a.rs", "MODIFIED")];
        assert_eq!(classify_pace(&sparse), "human");
    }

    /// The dominant directory becomes the scope hint only when most files
    /// share it; scattered changes get no misleading label.
    #[test]
    fn scope_hint_requires_a_dominant_directory() {
        let file = |path: &str| FileChange {
            path: path.to_string(),
            change: "modified".to_string(),
            event_count: 1,
            first_event_id: 1,
            last_event_id: 1,
            last_timestamp: 0,
            inserted: 0,
            deleted: 0,
            binary: false,
            old_path: None,
            ownership_status: "exclusive".to_string(),
            recoverable: true,
            warning: None,
        };
        let clustered = vec![
            file("src/auth/login.rs"),
            file("src/auth/session.rs"),
            file("README.md"),
        ];
        assert_eq!(scope_hint(&clustered).as_deref(), Some("src/auth"));

        let scattered = vec![
            file("src/auth/login.rs"),
            file("docs/notes.md"),
            file("lib/util/mod.rs"),
        ];
        assert_eq!(scope_hint(&scattered), None);

        assert_eq!(scope_hint(&[file("src/auth/login.rs")]), None);
    }

    /// A recent un-attributed group that deleted multiple files raises the
    /// panic alert; attributed Runs never do, even destructive ones.
    #[test]
    fn panic_alert_targets_recent_destructive_unattributed_groups() {
        let now = 1_000_000;
        let base = |kind: &str, deleted_files: usize, started_at: i64| TimelineItem {
            id: format!("g_{started_at}"),
            kind: kind.to_string(),
            label: "x".to_string(),
            actor: "unattributed".to_string(),
            agent: None,
            command: None,
            intent: None,
            status: "recorded".to_string(),
            started_at,
            ended_at: Some(started_at + 5),
            run_id: None,
            pace: "machine".to_string(),
            scope_hint: None,
            boundary_event_id: 0,
            last_event_id: 0,
            event_count: 10,
            file_count: 6,
            inserted: 0,
            deleted: 0,
            deleted_files,
            stats_truncated: false,
            files: Vec::new(),
            checkpoints: Vec::new(),
        };

        // Destructive run: no alert. Calm edits: no alert.
        assert!(
            panic_alert(
                &[base("run", 5, now - 60), base("edits", 1, now - 30)],
                now,
                None
            )
            .is_none()
        );

        // Two destructive edit groups: the newest wins; the target lands
        // just before it began.
        let alert = panic_alert(
            &[base("edits", 3, now - 600), base("edits", 2, now - 60)],
            now,
            None,
        )
        .unwrap();
        assert_eq!(alert.started_at, now - 60);
        assert_eq!(alert.target_timestamp, now - 61);
        assert_eq!(alert.deleted_files, 2);

        // Outside the 24-hour panic window: stale, no alert.
        assert!(
            panic_alert(&[base("edits", 4, now - PANIC_WINDOW_SECS - 10)], now, None).is_none()
        );

        // A recovery applied after the burst means the user already acted:
        // the alert retires instead of nagging forever.
        assert!(panic_alert(&[base("edits", 3, now - 600)], now, Some(now - 100)).is_none());
        // ...but a newer burst after that recovery still alerts.
        assert!(panic_alert(&[base("edits", 3, now - 50)], now, Some(now - 100)).is_some());
    }

    /// Runs claim their own events; everything else clusters into "edits"
    /// items, and both appear newest-first.
    #[test]
    fn timeline_separates_runs_from_unattributed_edits() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let db = Database::open_in_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let project = db.get_or_create_project(&root_path).unwrap();
        let file = root_path.join("a.rs").to_string_lossy().to_string();

        // One un-attributed edit before the run.
        db.insert_event(project.id, &file, "CREATED", None, None, None, None, None)
            .unwrap();
        // A completed agent Run with one change.
        let run = db
            .start_run(
                project.id,
                "agent-work",
                "run",
                "agent",
                Some("Claude Code"),
                Some("claude"),
                None,
                None,
            )
            .unwrap();
        db.insert_event(project.id, &file, "MODIFIED", None, None, None, None, None)
            .unwrap();
        db.complete_run(run.id, "completed").unwrap();

        let payload = timeline_payload(&db, &project, 100, None).unwrap();
        assert_eq!(payload.items.len(), 2);
        let kinds: Vec<&str> = payload
            .items
            .iter()
            .map(|item| item.kind.as_str())
            .collect();
        assert!(kinds.contains(&"run"));
        assert!(kinds.contains(&"edits"));
        let run_item = payload
            .items
            .iter()
            .find(|item| item.kind == "run")
            .unwrap();
        assert_eq!(run_item.label, "Claude Code");
        assert_eq!(run_item.event_count, 1);
        assert_eq!(run_item.file_count, 1);
    }

    #[test]
    fn timeline_emits_multi_claim_event_once_as_collision() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let db = Database::open_in_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let project = db.get_or_create_project(&root_path).unwrap();
        let file = root_path.join("shared.rs").to_string_lossy().into_owned();
        let first = db
            .start_reported_run(
                project.id,
                "first",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:timeline-first",
            )
            .unwrap();
        let second = db
            .start_reported_run(
                project.id,
                "second",
                "hook",
                "agent",
                Some("Codex"),
                None,
                None,
                "codex:timeline-second",
            )
            .unwrap();
        db.open_run_boundary(first.id, "first-change", std::slice::from_ref(&file))
            .unwrap();
        db.open_run_boundary(second.id, "second-change", std::slice::from_ref(&file))
            .unwrap();
        db.insert_event(project.id, &file, "MODIFIED", None, None, None, None, None)
            .unwrap();
        db.close_run_boundary(first.id, "first-change", std::slice::from_ref(&file))
            .unwrap();
        db.close_run_boundary(second.id, "second-change", std::slice::from_ref(&file))
            .unwrap();
        db.complete_run(first.id, "completed").unwrap();
        db.complete_run(second.id, "completed").unwrap();

        let payload = timeline_payload(&db, &project, 100, None).unwrap();
        let collisions = payload
            .items
            .iter()
            .filter(|item| item.kind == "collision")
            .collect::<Vec<_>>();
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].event_count, 1);
        assert_eq!(collisions[0].files[0].ownership_status, "collision");
        assert!(!collisions[0].files[0].recoverable);
        assert_eq!(
            payload
                .items
                .iter()
                .map(|item| item.event_count)
                .sum::<usize>(),
            1
        );
    }

    /// The diff endpoint reconstructs before/after content from snapshots and
    /// produces structured hunks with correct line numbers.
    #[test]
    fn diff_payload_builds_structured_hunks() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let db = Database::open_in_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let project = db.get_or_create_project(&root_path).unwrap();
        let file = root_path.join("a.rs").to_string_lossy().to_string();

        let old_content = b"line one\nline two\nline three\n";
        let new_content = b"line one\nline 2\nline three\n";
        let old_hash = snapshots::hash_bytes(old_content);
        let new_hash = snapshots::hash_bytes(new_content);
        let guard = snapshots::acquire_publish_guard().unwrap();
        snapshots::save_durable(&guard, project.id, &old_hash, old_content).unwrap();
        snapshots::save_durable(&guard, project.id, &new_hash, new_content).unwrap();

        db.insert_event(
            project.id,
            &file,
            "MODIFIED",
            Some(&new_hash),
            Some(&old_hash),
            None,
            None,
            Some(new_content.len() as i64),
        )
        .unwrap();
        let event_id = db.max_event_id(project.id).unwrap();

        let payload = diff_payload(&db, &project, "a.rs", event_id, event_id).unwrap();
        assert!(!payload.binary);
        assert_eq!(payload.inserted, 1);
        assert_eq!(payload.deleted, 1);
        assert_eq!(payload.hunks.len(), 1);
        let lines: Vec<&str> = payload.hunks[0]
            .lines
            .iter()
            .map(|line| line.kind.as_str())
            .collect();
        assert_eq!(lines, vec!["ctx", "del", "add", "ctx"]);
        assert_eq!(payload.hunks[0].lines[1].text, "line two");
        assert_eq!(payload.hunks[0].lines[2].text, "line 2");
    }

    struct PreviewFixture {
        _data: tempfile::TempDir,
        _tree: tempfile::TempDir,
        db: Database,
        project: WatchedProject,
    }

    impl PreviewFixture {
        fn new() -> Self {
            let data = tempfile::tempdir().unwrap();
            crate::set_test_data_dir(data.path().to_path_buf());
            let tree = tempfile::tempdir().unwrap();
            let db = Database::open().unwrap();
            let project = db
                .get_or_create_project(&tree.path().canonicalize().unwrap())
                .unwrap();
            Self {
                _data: data,
                _tree: tree,
                db,
                project,
            }
        }

        fn record(&self, path: &str, at: i64, before: Option<&[u8]>, after: Option<&[u8]>) {
            let guard = snapshots::acquire_publish_guard().unwrap();
            let save = |content: &[u8]| {
                let hash = snapshots::hash_bytes(content);
                snapshots::save_durable(&guard, self.project.id, &hash, content).unwrap();
                hash
            };
            let previous = before.map(save);
            let current = after.map(save);
            let absolute = Path::new(&self.project.root_path).join(path);
            self.db
                .insert_event(
                    self.project.id,
                    absolute.to_str().unwrap(),
                    if after.is_none() {
                        "DELETED"
                    } else if before.is_none() {
                        "CREATED"
                    } else {
                        "MODIFIED"
                    },
                    current.as_deref(),
                    previous.as_deref(),
                    None,
                    None,
                    after.map(|value| value.len() as i64),
                )
                .unwrap();
            self.db
                .set_event_timestamp(self.db.max_event_id(self.project.id).unwrap(), at)
                .unwrap();
            if let Some(content) = after {
                std::fs::write(absolute, content).unwrap();
            } else {
                let _ = std::fs::remove_file(absolute);
            }
        }

        fn preview(&self, at: i64, path: &str) -> RestorePreviewPayload {
            restore_preview_payload(&self.db, &self.project, at, Some(path)).unwrap()
        }
    }

    #[test]
    fn restore_preview_tracks_time_boundaries_and_current_to_target_direction() {
        let f = PreviewFixture::new();
        f.record("a.rs", 100, None, Some(b"one\n"));
        f.record("a.rs", 200, Some(b"one\n"), Some(b"two\n"));
        f.record("a.rs", 300, Some(b"two\n"), Some(b"three\n"));
        for (at, expected) in [(150, "one"), (200, "two"), (299, "two")] {
            let preview = f.preview(at, "a.rs");
            assert_eq!(preview.entries.len(), 1);
            let diff = preview.diff.unwrap();
            let lines = &diff.hunks[0].lines;
            assert!(
                lines
                    .iter()
                    .any(|line| line.kind == "del" && line.text == "three")
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.kind == "add" && line.text == expected)
            );
            assert_eq!(diff.new_timestamp, Some(at));
        }
        let now = f.preview(300, "a.rs");
        assert!(now.entries.is_empty());
        assert_eq!(now.diff.unwrap().change, "unchanged");
        assert_eq!(
            std::fs::read(Path::new(&f.project.root_path).join("a.rs")).unwrap(),
            b"three\n"
        );
        let conn = rusqlite::Connection::open(f._data.path().join("database.db")).unwrap();
        let saved: i64 = conn
            .query_row("SELECT COUNT(*) FROM recoveries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(saved, 0, "scrubbing must not persist recovery plans");
    }

    #[test]
    fn restore_preview_matches_saved_plan_and_selected_file_deletion() {
        let f = PreviewFixture::new();
        f.record("created.txt", 200, None, Some(b"new\n"));
        f.record("deleted.txt", 100, None, Some(b"bring back\n"));
        f.record("deleted.txt", 200, Some(b"bring back\n"), None);
        f.record("same.txt", 100, None, Some(b"same\n"));
        f.record("same.txt", 200, Some(b"same\n"), Some(b"temporary\n"));
        f.record("same.txt", 300, Some(b"temporary\n"), Some(b"same\n"));
        let preview = f.preview(150, "created.txt");
        assert_eq!(
            preview.entries.len(),
            2,
            "exclude edits that already match the target"
        );
        assert_eq!(preview.diff.unwrap().change, "deleted");
        assert_eq!(
            f.preview(150, "deleted.txt").diff.unwrap().change,
            "created"
        );
        assert_eq!(f.preview(150, "same.txt").diff.unwrap().change, "unchanged");
        let recovery = crate::recoveries::create_timestamp_recovery_in(
            &f.db,
            &f.project,
            Path::new(&f.project.root_path),
            ".",
            150,
            "test",
            "web",
        )
        .unwrap();
        let saved = recovery_view(&f.db, &f.project, &recovery).unwrap();
        assert_eq!(
            serde_json::to_value(preview.entries).unwrap(),
            serde_json::to_value(saved.entries).unwrap()
        );
        let selected = crate::recoveries::create_selected_timestamp_recovery_in(
            &f.db,
            &f.project,
            150,
            &["created.txt".into()],
            "test",
        )
        .unwrap();
        let selected = f.db.get_recovery_entries(selected.id).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "DELETE");
        assert!(Path::new(&f.project.root_path).join("created.txt").exists());
        assert!(!Path::new(&f.project.root_path).join("deleted.txt").exists());
    }

    #[test]
    fn restore_preview_covers_both_sides_of_rename() {
        let f = PreviewFixture::new();
        f.record("before.txt", 100, None, Some(b"contents\n"));
        let root = Path::new(&f.project.root_path);
        let old = root.join("before.txt");
        let new = root.join("after.txt");
        std::fs::rename(&old, &new).unwrap();
        let hash = snapshots::hash_bytes(b"contents\n");
        f.db.insert_event(
            f.project.id,
            new.to_str().unwrap(),
            "RENAMED",
            Some(&hash),
            Some(&hash),
            None,
            old.to_str(),
            Some(9),
        )
        .unwrap();
        f.db.set_event_timestamp(f.db.max_event_id(f.project.id).unwrap(), 200)
            .unwrap();
        assert_eq!(f.preview(150, "before.txt").diff.unwrap().change, "created");
        let preview = f.preview(150, "after.txt");
        assert_eq!(preview.entries.len(), 2);
        assert_eq!(preview.diff.unwrap().change, "deleted");
        assert!(f.preview(200, "after.txt").entries.is_empty());
    }

    #[test]
    fn restore_preview_handles_binary_empty_and_missing_snapshots_honestly() {
        let f = PreviewFixture::new();
        f.record("binary.dat", 100, None, Some(b"\0old"));
        f.record("binary.dat", 200, Some(b"\0old"), Some(b"\0new"));
        assert!(f.preview(150, "binary.dat").diff.unwrap().binary);
        assert_eq!(
            f.preview(200, "binary.dat").diff.unwrap().change,
            "unchanged"
        );
        f.record("empty.txt", 200, None, Some(b""));
        assert_eq!(f.preview(150, "empty.txt").diff.unwrap().change, "deleted");
        let hash = snapshots::hash_bytes(b"\0old");
        std::fs::remove_file(
            f._data
                .path()
                .join("snapshots")
                .join(f.project.id.to_string())
                .join(format!("{hash}.gz")),
        )
        .unwrap();
        assert!(restore_preview_payload(&f.db, &f.project, 150, Some("binary.dat")).is_err());
    }

    #[test]
    fn restore_preview_rejects_paths_outside_project() {
        let f = PreviewFixture::new();
        for path in ["../outside", "/etc/passwd", "a/../../outside"] {
            assert!(restore_preview_payload(&f.db, &f.project, 150, Some(path)).is_err());
            assert!(
                crate::recoveries::create_selected_timestamp_recovery_in(
                    &f.db,
                    &f.project,
                    150,
                    &[path.into()],
                    "test",
                )
                .is_err()
            );
        }
    }
}
