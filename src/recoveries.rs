use anyhow::{Context, Result};
use chrono::Utc;
use diffy::{apply_bytes, create_patch_bytes};
use std::collections::BTreeSet;
use std::path::Path;

use crate::db::Database;
use crate::models::{FileEvent, Recovery, RecoveryEntry, RunIntent, Session, WatchedProject};
use crate::restore::{
    self, ExpectedState, RestoreAction, RestoreKind, RestorePlan, RestorePlanEntry, RestoreSource,
};
use crate::{BOLD, DIM, GREEN, RESET, YELLOW, find_project, snapshots};

enum StoredState {
    Present { hash: String, timestamp: i64 },
    Absent,
    Unknown,
}

struct CurrentState {
    exists: bool,
    hash: Option<String>,
    content: Option<Vec<u8>>,
}

struct RecoverySpec<'a> {
    run_id: Option<i64>,
    request: &'a str,
    kind: &'a str,
    confidence: &'a str,
    ambiguity: Option<&'a str>,
}

pub fn create_run_recovery(
    run: &Session,
    paths: &[String],
    request: &str,
    kind: &str,
    confidence: &str,
    ambiguity: Option<&str>,
) -> Result<Recovery> {
    if run.is_active() {
        anyhow::bail!(
            "No recovery plan was created because Run {} is still active.\nFinish the Run, then try again.",
            run.public_id(),
        );
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    ensure_run_project(run, &project)?;
    let plan = restore::plan_paths_restore_at_session_start(&db, &project, &cwd, paths, run)?;
    let recovery = persist_restore_plan(
        &db,
        &project,
        RecoverySpec {
            run_id: Some(run.id),
            request,
            kind,
            confidence,
            ambiguity,
        },
        &plan,
    )?;
    print_recovery(&db, &project, &recovery)?;
    Ok(recovery)
}

pub fn create_timestamp_recovery(
    path: &str,
    target_timestamp: i64,
    request: &str,
    kind: &str,
) -> Result<Recovery> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let plan = restore::plan_restore(&db, &project, &cwd, path, target_timestamp, false)?;
    let recovery = persist_restore_plan(
        &db,
        &project,
        RecoverySpec {
            run_id: None,
            request,
            kind,
            confidence: "exact-timestamp",
            ambiguity: None,
        },
        &plan,
    )?;
    print_recovery(&db, &project, &recovery)?;
    Ok(recovery)
}

pub fn create_intent_recovery(
    run: &Session,
    intent: &RunIntent,
    request: &str,
) -> Result<Recovery> {
    if run.is_active() {
        anyhow::bail!(
            "No recovery plan was created because Run {} is still active.\nFinish the Run, then try again.",
            run.public_id(),
        );
    }
    let end_event_id = intent.end_event_id.ok_or_else(|| {
        anyhow::anyhow!(
            "No recovery plan was created because task '{}' is still active.\nFinish the task boundary, then try again.",
            intent.label,
        )
    })?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    ensure_run_project(run, &project)?;
    let events = db.get_events_between_ids(project.id, intent.start_event_id, end_event_id)?;
    let paths = paths_from_events(&events);

    let mut plan_entries = Vec::new();
    let mut conflicts = Vec::new();
    for path in paths {
        match plan_inverse_intent_path(&db, &project, &path, intent.start_event_id, end_event_id) {
            Ok(Some(entry)) => plan_entries.push(entry),
            Ok(None) => {}
            Err(error) => conflicts.push(format!(
                "{}: {}",
                crate::relative_path(&path, &project.root_path),
                error
            )),
        }
    }
    plan_entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let ambiguity = (!conflicts.is_empty()).then(|| conflicts.join("; "));
    let confidence = if ambiguity.is_some() {
        "ambiguous"
    } else {
        "explicit-intent"
    };
    let plan = RestorePlan {
        entries: plan_entries,
    };
    let recovery = persist_restore_plan(
        &db,
        &project,
        RecoverySpec {
            run_id: Some(run.id),
            request,
            kind: "intent",
            confidence,
            ambiguity: ambiguity.as_deref(),
        },
        &plan,
    )?;
    print_recovery(&db, &project, &recovery)?;
    Ok(recovery)
}

pub fn cmd_apply(reference: &str) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let recovery = db
        .get_recovery_by_ref(project.id, reference)?
        .ok_or_else(|| anyhow::anyhow!("Recovery '{}' not found", reference))?;

    if recovery.status == "applied" {
        println!(
            "{}Saved recovery plan {} was already applied; no files changed.{}",
            GREEN,
            recovery.public_id(),
            RESET
        );
        return Ok(());
    }
    if recovery.status != "planned" {
        anyhow::bail!(
            "Saved recovery plan {} cannot be applied because its status is '{}'.",
            recovery.public_id(),
            recovery.status
        );
    }
    if Utc::now().timestamp() > recovery.expires_at {
        anyhow::bail!(
            "No files changed because saved recovery plan {} expired.\nCreate a new preview, then apply the new plan.",
            recovery.public_id(),
        );
    }
    if let Some(ambiguity) = &recovery.ambiguity {
        anyhow::bail!(
            "No files changed because saved recovery plan {} has overlapping changes.\nReview this conflict before trying another recovery: {}",
            recovery.public_id(),
            ambiguity
        );
    }

    let entries = db.get_recovery_entries(recovery.id)?;
    let conflicts = preflight_entries(&project, &entries)?;
    if !conflicts.is_empty() {
        let reason = conflicts.join("; ");
        db.mark_recovery_conflicted(recovery.id, &reason)?;
        anyhow::bail!(
            "No files changed because files were edited after saved recovery plan {} was created.\nCreate a new preview for: {}",
            recovery.public_id(),
            reason
        );
    }

    let plan = recovery_entries_to_plan(&project, &entries)?;
    restore::apply_restore_plan(&project, &plan, true)?;
    db.mark_recovery_applied(recovery.id)?;
    let file_label = if entries.len() == 1 { "file" } else { "files" };
    println!(
        "{}Changed {} {} using saved recovery plan {}.{}",
        GREEN,
        entries.len(),
        file_label,
        recovery.public_id(),
        RESET
    );
    Ok(())
}

pub fn print_recovery(db: &Database, project: &WatchedProject, recovery: &Recovery) -> Result<()> {
    let entries = db.get_recovery_entries(recovery.id)?;
    println!(
        "{}Saved recovery plan {}.{}",
        BOLD,
        recovery.public_id(),
        RESET
    );
    if let Some(run_id) = recovery.run_id {
        println!("Run:        r_{}", run_id);
    }
    println!("Request:    {}", recovery.request);
    println!("Match:      {}", confidence_label(&recovery.confidence));
    match recovery.confidence.as_str() {
        "explicit-intent" => {
            println!("Keeps:      later file changes that do not overlap");
        }
        "path-match" => {
            println!("Scope:      whole files matched by file and folder names");
        }
        _ => {}
    }
    println!("Files:      {}", entries.len());
    if let Some(ambiguity) = &recovery.ambiguity {
        println!("{}Needs review:{} {}", YELLOW, RESET, ambiguity);
    }
    println!();

    let plan = recovery_entries_to_plan(project, &entries)?;
    if plan.entries.is_empty() {
        println!("This plan would not change any files.");
        println!("{}No files changed.{}", DIM, RESET);
    } else {
        restore::print_restore_plan(project, &plan, &recovery.public_id())?;
    }
    println!(
        "Apply this exact plan with: undo apply {}",
        recovery.public_id()
    );
    Ok(())
}

fn confidence_label(confidence: &str) -> &str {
    match confidence {
        "explicit-intent" => "High — matched a completed task boundary",
        "path-match" => "Medium — matched file and folder names",
        "exact-paths" => "High — exact selected files",
        "exact-timestamp" => "High — exact saved time",
        "ambiguous" => "Needs review — changes overlap",
        other => other,
    }
}

fn persist_restore_plan(
    db: &Database,
    project: &WatchedProject,
    spec: RecoverySpec<'_>,
    plan: &RestorePlan,
) -> Result<Recovery> {
    let mut entries = Vec::new();
    for entry in &plan.entries {
        let path = Path::new(&entry.path);
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            anyhow::bail!(
                "Cannot create a recovery plan through symlink '{}'.",
                entry.rel_path
            );
        }
        let current = read_current_state(path)?;
        let expected = entry
            .expected_current
            .clone()
            .unwrap_or_else(|| ExpectedState {
                exists: current.exists,
                hash: current.hash.clone(),
            });
        if current.exists != expected.exists || current.hash != expected.hash {
            anyhow::bail!(
                "No recovery plan was saved because {} changed during the preview.\nRun the preview again.",
                entry.rel_path
            );
        }
        let (action, target_hash, source_timestamp) = match &entry.action {
            RestoreAction::Write { source } => {
                if expected.exists && expected.hash.as_deref() == Some(source.hash.as_str()) {
                    continue;
                }
                (
                    "WRITE".to_string(),
                    Some(source.hash.clone()),
                    Some(source.timestamp),
                )
            }
            RestoreAction::DeleteCreatedAfterTarget => {
                if !expected.exists {
                    continue;
                }
                ("DELETE".to_string(), None, None)
            }
        };
        entries.push(RecoveryEntry {
            recovery_id: 0,
            path: entry.path.clone(),
            action,
            target_hash,
            source_timestamp,
            expected_hash: expected.hash,
            expected_exists: expected.exists,
        });
    }
    db.create_recovery(
        project.id,
        spec.run_id,
        spec.request,
        spec.kind,
        spec.confidence,
        spec.ambiguity,
        &entries,
    )
}

fn preflight_entries(project: &WatchedProject, entries: &[RecoveryEntry]) -> Result<Vec<String>> {
    let mut conflicts = Vec::new();
    for entry in entries {
        let path = Path::new(&entry.path);
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            conflicts.push(format!(
                "{} (is now a symlink)",
                crate::relative_path(&entry.path, &project.root_path)
            ));
            continue;
        }
        crate::safe_resolve_path(
            Path::new(&project.root_path),
            &entry.path,
            &project.root_path,
        )?;
        let current = read_current_state(path)?;
        if current.exists != entry.expected_exists || current.hash != entry.expected_hash {
            conflicts.push(crate::relative_path(&entry.path, &project.root_path).to_string());
            continue;
        }
        if let Some(hash) = &entry.target_hash {
            snapshots::load(project.id, hash).with_context(|| {
                format!(
                    "Recovery source for {} is unavailable",
                    crate::relative_path(&entry.path, &project.root_path)
                )
            })?;
        }
    }
    Ok(conflicts)
}

fn recovery_entries_to_plan(
    project: &WatchedProject,
    entries: &[RecoveryEntry],
) -> Result<RestorePlan> {
    let mut plan_entries = Vec::new();
    for entry in entries {
        let action = match entry.action.as_str() {
            "WRITE" => RestoreAction::Write {
                source: RestoreSource {
                    hash: entry.target_hash.clone().ok_or_else(|| {
                        anyhow::anyhow!("WRITE Recovery entry has no target hash")
                    })?,
                    timestamp: entry
                        .source_timestamp
                        .unwrap_or_else(|| Utc::now().timestamp()),
                    kind: RestoreKind::Exact,
                },
            },
            "DELETE" => RestoreAction::DeleteCreatedAfterTarget,
            other => anyhow::bail!("unknown Recovery action '{}'", other),
        };
        plan_entries.push(RestorePlanEntry {
            path: entry.path.clone(),
            rel_path: crate::relative_path(&entry.path, &project.root_path).to_string(),
            action,
            expected_current: None,
        });
    }
    Ok(RestorePlan {
        entries: plan_entries,
    })
}

fn plan_inverse_intent_path(
    db: &Database,
    project: &WatchedProject,
    path: &str,
    start_event_id: i64,
    end_event_id: i64,
) -> Result<Option<RestorePlanEntry>> {
    let before = stored_state_at_id(db, project.id, path, start_event_id)?;
    let after = stored_state_at_id(db, project.id, path, end_event_id)?;
    if same_stored_state(&before, &after) {
        return Ok(None);
    }
    let current = read_current_state(Path::new(path))?;
    let expected_current = ExpectedState {
        exists: current.exists,
        hash: current.hash.clone(),
    };
    let rel_path = crate::relative_path(path, &project.root_path).to_string();

    if current_matches_stored(&current, &after) {
        return direct_entry_from_state(path, rel_path, before, expected_current);
    }

    let (
        StoredState::Present {
            hash: before_hash,
            timestamp,
        },
        StoredState::Present {
            hash: after_hash, ..
        },
        Some(current_content),
    ) = (before, after, current.content)
    else {
        anyhow::bail!("later edits overlap a create, delete, rename, or unavailable snapshot");
    };

    let before_content = snapshots::load(project.id, &before_hash)?;
    let after_content = snapshots::load(project.id, &after_hash)?;
    let inverse = create_patch_bytes(&after_content, &before_content);
    let target = apply_bytes(&current_content, &inverse)
        .map_err(|_| anyhow::anyhow!("later edits overlap the unwanted intent"))?;
    let target_hash = snapshots::hash_bytes(&target);
    if current.hash.as_deref() == Some(target_hash.as_str()) {
        return Ok(None);
    }
    snapshots::save_durable(project.id, &target_hash, &target)?;
    Ok(Some(RestorePlanEntry {
        path: path.to_string(),
        rel_path,
        action: RestoreAction::Write {
            source: RestoreSource {
                hash: target_hash,
                timestamp,
                kind: RestoreKind::Exact,
            },
        },
        expected_current: Some(expected_current),
    }))
}

fn direct_entry_from_state(
    path: &str,
    rel_path: String,
    state: StoredState,
    expected_current: ExpectedState,
) -> Result<Option<RestorePlanEntry>> {
    match state {
        StoredState::Present { hash, timestamp } => Ok(Some(RestorePlanEntry {
            path: path.to_string(),
            rel_path,
            action: RestoreAction::Write {
                source: RestoreSource {
                    hash,
                    timestamp,
                    kind: RestoreKind::Exact,
                },
            },
            expected_current: Some(expected_current),
        })),
        StoredState::Absent => Ok(Some(RestorePlanEntry {
            path: path.to_string(),
            rel_path,
            action: RestoreAction::DeleteCreatedAfterTarget,
            expected_current: Some(expected_current),
        })),
        StoredState::Unknown => anyhow::bail!("state before intent is no longer recoverable"),
    }
}

fn stored_state_at_id(
    db: &Database,
    project_id: i64,
    path: &str,
    event_id: i64,
) -> Result<StoredState> {
    if let Some(event) = db.get_path_state_event_at_id(project_id, path, event_id)? {
        if event.old_path.as_deref() == Some(path) && event.path != path {
            return Ok(StoredState::Absent);
        }
        if event.path == path && event.event_type == "DELETED" {
            return Ok(StoredState::Absent);
        }
        return Ok(match event.current_hash {
            Some(hash) => StoredState::Present {
                hash,
                timestamp: event.timestamp,
            },
            None => StoredState::Unknown,
        });
    }
    if let Some(event) = db.get_first_path_event_after_id(project_id, path, event_id)? {
        let existed = event.old_path.as_deref() == Some(path)
            || (event.path == path && matches!(event.event_type.as_str(), "MODIFIED" | "DELETED"));
        if existed {
            return Ok(match event.previous_hash {
                Some(hash) => StoredState::Present {
                    hash,
                    timestamp: event.timestamp,
                },
                None => StoredState::Unknown,
            });
        }
        return Ok(StoredState::Absent);
    }
    Ok(StoredState::Unknown)
}

fn same_stored_state(left: &StoredState, right: &StoredState) -> bool {
    match (left, right) {
        (StoredState::Present { hash: left, .. }, StoredState::Present { hash: right, .. }) => {
            left == right
        }
        (StoredState::Absent, StoredState::Absent)
        | (StoredState::Unknown, StoredState::Unknown) => true,
        _ => false,
    }
}

fn current_matches_stored(current: &CurrentState, stored: &StoredState) -> bool {
    match stored {
        StoredState::Present { hash, .. } => {
            current.exists && current.hash.as_deref() == Some(hash.as_str())
        }
        StoredState::Absent => !current.exists,
        StoredState::Unknown => false,
    }
}

fn read_current_state(path: &Path) -> Result<CurrentState> {
    if !path.exists() {
        return Ok(CurrentState {
            exists: false,
            hash: None,
            content: None,
        });
    }
    let content = crate::diff::read_capped(path, snapshots::MAX_SNAPSHOT_SIZE)?
        .ok_or_else(|| anyhow::anyhow!("{} exceeds Undo's snapshot limit", path.display()))?;
    Ok(CurrentState {
        exists: true,
        hash: Some(snapshots::hash_bytes(&content)),
        content: Some(content),
    })
}

fn paths_from_events(events: &[FileEvent]) -> Vec<String> {
    events
        .iter()
        .flat_map(|event| std::iter::once(event.path.clone()).chain(event.old_path.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn ensure_run_project(run: &Session, project: &WatchedProject) -> Result<()> {
    if run.project_id != project.id {
        anyhow::bail!("Run {} belongs to a different project", run.public_id());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_state_matches_present_and_absent_states() {
        let present = CurrentState {
            exists: true,
            hash: Some("abc".to_string()),
            content: Some(Vec::new()),
        };
        assert!(current_matches_stored(
            &present,
            &StoredState::Present {
                hash: "abc".to_string(),
                timestamp: 1,
            }
        ));
        let absent = CurrentState {
            exists: false,
            hash: None,
            content: None,
        };
        assert!(current_matches_stored(&absent, &StoredState::Absent));
    }

    #[test]
    fn inverse_patch_preserves_non_overlapping_later_changes() {
        let before = b"header\nauth = old\nfooter\n";
        let after = b"header\nauth = agent\nfooter\n";
        let current = b"header\nauth = agent\nfooter\ndashboard = kept\n";
        let inverse = create_patch_bytes(after, before);
        let result = apply_bytes(current, &inverse).unwrap();
        assert_eq!(result, b"header\nauth = old\nfooter\ndashboard = kept\n");
    }

    #[test]
    fn inverse_patch_rejects_overlapping_later_changes() {
        let before = b"auth = old\n";
        let after = b"auth = agent\n";
        let current = b"auth = human-followup\n";
        let inverse = create_patch_bytes(after, before);
        assert!(apply_bytes(current, &inverse).is_err());
    }

    #[test]
    fn preflight_rejects_file_changed_after_preview() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("auth.rs");
        std::fs::write(&file, "previewed").unwrap();
        let expected = snapshots::hash_bytes(b"previewed");
        let project = WatchedProject {
            id: 1,
            root_path: root.to_string_lossy().to_string(),
            created_at: 0,
        };
        let entry = RecoveryEntry {
            recovery_id: 1,
            path: file.to_string_lossy().to_string(),
            action: "DELETE".to_string(),
            target_hash: None,
            source_timestamp: None,
            expected_hash: Some(expected),
            expected_exists: true,
        };
        std::fs::write(&file, "changed later").unwrap();

        let conflicts = preflight_entries(&project, &[entry]).unwrap();
        assert_eq!(conflicts, vec!["auth.rs"]);
    }

    #[test]
    fn confidence_values_are_humanized_only_for_display() {
        assert_eq!(
            confidence_label("explicit-intent"),
            "High — matched a completed task boundary"
        );
        assert_eq!(
            confidence_label("path-match"),
            "Medium — matched file and folder names"
        );
        assert_eq!(
            confidence_label("exact-paths"),
            "High — exact selected files"
        );
        assert_eq!(
            confidence_label("exact-timestamp"),
            "High — exact saved time"
        );
        assert_eq!(
            confidence_label("ambiguous"),
            "Needs review — changes overlap"
        );
        assert_eq!(confidence_label("future-value"), "future-value");
    }
}
