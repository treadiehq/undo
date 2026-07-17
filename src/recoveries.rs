use anyhow::{Context, Result};
use chrono::Utc;
use diffy::{apply_bytes, create_patch_bytes};
use std::collections::BTreeSet;

use crate::db::Database;
use crate::models::{FileEvent, Recovery, RecoveryEntry, RunIntent, Session, WatchedProject};
use crate::restore::{
    self, ExpectedState, RestoreAction, RestoreKind, RestorePlan, RestorePlanEntry, RestoreSource,
};
use crate::restore_fs::{CappedRead, ProjectPath, RestoreFs};
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
    let fs = RestoreFs::open(&project)?;
    let publish_guard = snapshots::acquire_publish_guard()?;

    let mut plan_entries = Vec::new();
    let mut conflicts = Vec::new();
    for path in paths {
        let target = ProjectPath::from_stored(&project, &path)?;
        fs.validate(&target)?;
        match plan_inverse_intent_path(
            &db,
            &project,
            &fs,
            &publish_guard,
            &target,
            &path,
            (intent.start_event_id, end_event_id),
        ) {
            Ok(Some(entry)) => plan_entries.push(entry),
            Ok(None) => {}
            Err(error) => conflicts.push(format!("{}: {}", target.display(), error)),
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
    drop(publish_guard);
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
    let fs = RestoreFs::open(&project)?;
    let conflicts = preflight_entries(&project, &fs, &entries)?;
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
    if let Err(error) = restore::apply_restore_plan_with_fs(&project, &plan, &fs) {
        let reason = format!("apply stopped: {error}");
        db.mark_recovery_conflicted(recovery.id, &reason)?;
        return Err(error).with_context(|| {
            format!(
                "Saved recovery plan {} stopped during apply; earlier files in the plan may have changed.",
                recovery.public_id()
            )
        });
    }
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
    let fs = RestoreFs::open(project)?;
    let mut entries = Vec::new();
    for entry in &plan.entries {
        fs.validate(&entry.path)?;
        let current = read_current_state(&fs, &entry.path)?;
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
            path: entry.path.absolute(project).to_string_lossy().to_string(),
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

fn preflight_entries(
    project: &WatchedProject,
    fs: &RestoreFs,
    entries: &[RecoveryEntry],
) -> Result<Vec<String>> {
    let mut conflicts = Vec::new();
    for entry in entries {
        let target = ProjectPath::from_stored(project, &entry.path)?;
        if let Err(error) = fs.validate(&target) {
            conflicts.push(format!("{} ({})", target.display(), error));
            continue;
        }
        let current = read_current_state(fs, &target)?;
        if current.exists != entry.expected_exists || current.hash != entry.expected_hash {
            conflicts.push(target.display());
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
        let target = ProjectPath::from_stored(project, &entry.path)?;
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
            path: target.clone(),
            rel_path: target.display(),
            action,
            expected_current: Some(ExpectedState {
                exists: entry.expected_exists,
                hash: entry.expected_hash.clone(),
            }),
        });
    }
    Ok(RestorePlan {
        entries: plan_entries,
    })
}

fn plan_inverse_intent_path(
    db: &Database,
    project: &WatchedProject,
    fs: &RestoreFs,
    publish_guard: &snapshots::PublishGuard,
    target: &ProjectPath,
    db_path: &str,
    event_ids: (i64, i64),
) -> Result<Option<RestorePlanEntry>> {
    let (start_event_id, end_event_id) = event_ids;
    let before = stored_state_at_id(db, project.id, db_path, start_event_id)?;
    let after = stored_state_at_id(db, project.id, db_path, end_event_id)?;
    if same_stored_state(&before, &after) {
        return Ok(None);
    }
    let current = read_current_state(fs, target)?;
    let expected_current = ExpectedState {
        exists: current.exists,
        hash: current.hash.clone(),
    };
    let rel_path = target.display();

    if current_matches_stored(&current, &after) {
        return direct_entry_from_state(target, rel_path, before, expected_current);
    }
    if current_matches_stored(&current, &before) {
        return Ok(None);
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
    let target_content = apply_bytes(&current_content, &inverse)
        .map_err(|_| anyhow::anyhow!("later edits overlap the unwanted intent"))?;
    let target_hash = snapshots::hash_bytes(&target_content);
    if current.hash.as_deref() == Some(target_hash.as_str()) {
        return Ok(None);
    }
    snapshots::save_durable(publish_guard, project.id, &target_hash, &target_content)?;
    Ok(Some(RestorePlanEntry {
        path: target.clone(),
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
    target: &ProjectPath,
    rel_path: String,
    state: StoredState,
    expected_current: ExpectedState,
) -> Result<Option<RestorePlanEntry>> {
    match state {
        StoredState::Present { hash, timestamp } => Ok(Some(RestorePlanEntry {
            path: target.clone(),
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
            path: target.clone(),
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

fn read_current_state(fs: &RestoreFs, target: &ProjectPath) -> Result<CurrentState> {
    let content = match fs.read_capped(target, snapshots::MAX_SNAPSHOT_SIZE)? {
        CappedRead::Missing => {
            return Ok(CurrentState {
                exists: false,
                hash: None,
                content: None,
            });
        }
        CappedRead::TooLarge => {
            anyhow::bail!(
                "{} exceeds Undo's snapshot limit",
                target.relative().display()
            )
        }
        CappedRead::Content(content) => content,
    };
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
    fn intent_recovery_skips_file_already_reverted_to_before_state() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("config.txt");
        let before = b"setting = before\n";
        let after = b"setting = after\n";
        std::fs::write(&file, before).unwrap();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        let before_hash = snapshots::hash_bytes(before);
        let after_hash = snapshots::hash_bytes(after);
        let publish_guard = snapshots::acquire_publish_guard().unwrap();
        snapshots::save_durable(&publish_guard, project.id, &before_hash, before).unwrap();
        snapshots::save_durable(&publish_guard, project.id, &after_hash, after).unwrap();
        let path = file.to_string_lossy().to_string();
        let start_event_id = db.max_event_id(project.id).unwrap();
        db.insert_event(
            project.id,
            &path,
            "MODIFIED",
            Some(&after_hash),
            Some(&before_hash),
            None,
            None,
            Some(after.len() as i64),
        )
        .unwrap();
        let end_event_id = db.max_event_id(project.id).unwrap();
        let fs = RestoreFs::open(&project).unwrap();
        let target = ProjectPath::from_stored(&project, &path).unwrap();

        let entry = plan_inverse_intent_path(
            &db,
            &project,
            &fs,
            &publish_guard,
            &target,
            &path,
            (start_event_id, end_event_id),
        )
        .unwrap();

        assert!(entry.is_none(), "already-reverted file needs no recovery");
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

        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let fs = RestoreFs::open(&project).unwrap();
        let conflicts = preflight_entries(&project, &fs, &[entry]).unwrap();
        assert_eq!(conflicts, vec!["auth.rs"]);
    }

    #[test]
    fn preflight_rejects_target_through_outside_parent_symlink() {
        use std::os::unix::fs::symlink;

        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("auth.rs");
        std::fs::write(&outside_file, "outside").unwrap();
        symlink(outside.path(), root.join("linked")).unwrap();
        let project = WatchedProject {
            id: 1,
            root_path: root.to_string_lossy().to_string(),
            created_at: 0,
        };
        let entry = RecoveryEntry {
            recovery_id: 1,
            path: root.join("linked/auth.rs").to_string_lossy().to_string(),
            action: "DELETE".to_string(),
            target_hash: None,
            source_timestamp: None,
            expected_hash: Some(snapshots::hash_bytes(b"outside")),
            expected_exists: true,
        };
        let fs = RestoreFs::open(&project).unwrap();

        let conflicts = preflight_entries(&project, &fs, &[entry]).unwrap();

        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].starts_with("linked/auth.rs ("));
        assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "outside");
    }

    #[test]
    fn persisted_recovery_rejects_parent_components() {
        let project = WatchedProject {
            id: 1,
            root_path: "/project".to_string(),
            created_at: 0,
        };
        let entry = RecoveryEntry {
            recovery_id: 1,
            path: "/project/src/../../outside".to_string(),
            action: "DELETE".to_string(),
            target_hash: None,
            source_timestamp: None,
            expected_hash: None,
            expected_exists: false,
        };

        assert!(recovery_entries_to_plan(&project, &[entry]).is_err());
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
