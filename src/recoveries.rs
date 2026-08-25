use anyhow::{Context, Result};
use chrono::Utc;
use diffy::{apply_bytes, create_patch_bytes};
use std::collections::{BTreeMap, BTreeSet};

use crate::db::Database;
use crate::models::{FileEvent, Recovery, RecoveryEntry, RunIntent, Session, WatchedProject};
use crate::restore::{
    self, ExpectedState, RestoreAction, RestoreKind, RestorePlan, RestorePlanEntry, RestoreSource,
};
use crate::restore_fs::{CappedRead, ProjectPath, RestoreFs};
use crate::{BOLD, DIM, GREEN, RESET, YELLOW, resolve_project, snapshots};

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
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let recovery = create_run_recovery_in(
        &db, &project, &cwd, run, paths, request, kind, confidence, ambiguity,
    )?;
    print_recovery(&db, &project, &recovery)?;
    Ok(recovery)
}

/// Plan and persist a Run recovery against an explicit database, project, and
/// base directory. Unlike [`create_run_recovery`] this neither consults the
/// process working directory nor prints, so callers such as the local web UI
/// can serve any watched project.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_run_recovery_in(
    db: &Database,
    project: &WatchedProject,
    base_dir: &std::path::Path,
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
    ensure_run_project(run, project)?;
    ensure_reported_paths_recoverable(db, project, base_dir, run, paths)?;
    let plan = restore::plan_paths_restore_at_session_start(db, project, base_dir, paths, run)?;
    persist_restore_plan(
        db,
        project,
        RecoverySpec {
            run_id: Some(run.id),
            request,
            kind,
            confidence,
            ambiguity,
        },
        &plan,
    )
}

pub fn create_timestamp_recovery(
    path: &str,
    target_timestamp: i64,
    request: &str,
    kind: &str,
) -> Result<Recovery> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let recovery =
        create_timestamp_recovery_in(&db, &project, &cwd, path, target_timestamp, request, kind)?;
    print_recovery(&db, &project, &recovery)?;
    Ok(recovery)
}

/// Timestamp-recovery twin of [`create_run_recovery_in`]: explicit context,
/// no printing, usable from the web UI server.
pub(crate) fn create_timestamp_recovery_in(
    db: &Database,
    project: &WatchedProject,
    base_dir: &std::path::Path,
    path: &str,
    target_timestamp: i64,
    request: &str,
    kind: &str,
) -> Result<Recovery> {
    let plan = restore::plan_restore(db, project, base_dir, path, target_timestamp, false)?;
    persist_restore_plan(
        db,
        project,
        RecoverySpec {
            run_id: None,
            request,
            kind,
            confidence: "exact-timestamp",
            ambiguity: None,
        },
        &plan,
    )
}

pub fn create_event_boundary_recovery(
    paths: &[String],
    boundary_event_id: i64,
    request: &str,
    kind: &str,
) -> Result<Recovery> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let recovery = create_event_boundary_recovery_in(
        &db,
        &project,
        &cwd,
        paths,
        boundary_event_id,
        request,
        kind,
    )?;
    print_recovery(&db, &project, &recovery)?;
    Ok(recovery)
}

/// Restore selected paths to their state at an exact recorded change boundary.
/// Used by the web UI and `undo recover --before-change` to undo a group of
/// un-attributed file changes ("restore these files to how they were just
/// before change N").
pub(crate) fn create_event_boundary_recovery_in(
    db: &Database,
    project: &WatchedProject,
    base_dir: &std::path::Path,
    paths: &[String],
    event_id: i64,
    request: &str,
    kind: &str,
) -> Result<Recovery> {
    let plan = restore::plan_paths_restore_at_event_id(db, project, base_dir, paths, event_id)?;
    persist_restore_plan(
        db,
        project,
        RecoverySpec {
            run_id: None,
            request,
            kind,
            confidence: "exact-paths",
            ambiguity: None,
        },
        &plan,
    )
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
    let project = resolve_project(&db, &cwd)?;
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

pub struct AppliedRecovery {
    pub recovery: Recovery,
    pub files_changed: usize,
    pub already_applied: bool,
}

pub fn cmd_apply(reference: &str) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let outcome = apply_recovery_in(&db, &project, reference)?;
    if outcome.already_applied {
        println!(
            "{}Saved recovery plan {} was already applied; no files changed.{}",
            GREEN,
            outcome.recovery.public_id(),
            RESET
        );
        return Ok(());
    }
    let file_label = if outcome.files_changed == 1 {
        "file"
    } else {
        "files"
    };
    println!(
        "{}Changed {} {} using saved recovery plan {}.{}",
        GREEN,
        outcome.files_changed,
        file_label,
        outcome.recovery.public_id(),
        RESET
    );
    Ok(())
}

/// Apply a persisted recovery plan against an explicit database and project.
/// Carries the same expiry, ambiguity, and expected-hash preflight guarantees
/// as the CLI path; it only skips the printing.
pub(crate) fn apply_recovery_paths_in(
    db: &Database,
    project: &WatchedProject,
    reference: &str,
    paths: Option<&[String]>,
) -> Result<AppliedRecovery> {
    let Some(paths) = paths else {
        return apply_recovery_in(db, project, reference);
    };
    let source = db
        .get_recovery_by_ref(project.id, reference)?
        .ok_or_else(|| anyhow::anyhow!("Recovery '{}' not found", reference))?;
    let source_entries = db.get_recovery_entries(source.id)?;
    let selected_entries = select_recovery_entries(project, &source_entries, paths)?;

    // Applying any selection from an already-applied plan is still a no-op.
    if source.status == "applied" {
        return apply_recovery_in(db, project, reference);
    }
    ensure_recovery_is_applicable(&source)?;

    if selected_entries.len() == source_entries.len() {
        return apply_recovery_in(db, project, reference);
    }

    // A subset is a new immutable plan. Its apply/conflict status belongs to
    // the derived plan; the source preview remains available and unchanged.
    let request = format!(
        "{} ({} of {} selected files)",
        source.request,
        selected_entries.len(),
        source_entries.len()
    );
    let derived = db.create_recovery(
        project.id,
        source.run_id,
        &request,
        &source.kind,
        &source.confidence,
        source.ambiguity.as_deref(),
        &selected_entries,
    )?;
    apply_recovery_in(db, project, &derived.public_id())
}

pub(crate) fn apply_recovery_in(
    db: &Database,
    project: &WatchedProject,
    reference: &str,
) -> Result<AppliedRecovery> {
    let recovery = db
        .get_recovery_by_ref(project.id, reference)?
        .ok_or_else(|| anyhow::anyhow!("Recovery '{}' not found", reference))?;

    if recovery.status == "applied" {
        return Ok(AppliedRecovery {
            recovery,
            files_changed: 0,
            already_applied: true,
        });
    }
    ensure_recovery_is_applicable(&recovery)?;

    let entries = db.get_recovery_entries(recovery.id)?;
    let fs = RestoreFs::open(project)?;
    let conflicts = preflight_entries(project, &fs, &entries)?;
    if !conflicts.is_empty() {
        let reason = conflicts.join("; ");
        db.mark_recovery_conflicted(recovery.id, &reason)?;
        anyhow::bail!(
            "No files changed because files were edited after saved recovery plan {} was created.\nCreate a new preview for: {}",
            recovery.public_id(),
            reason
        );
    }

    let plan = recovery_entries_to_plan(project, &entries)?;
    if let Err(error) = restore::apply_restore_plan_with_fs(project, &plan, &fs) {
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
    let applied_recovery = db
        .get_recovery_by_ref(project.id, &recovery.public_id())?
        .ok_or_else(|| anyhow::anyhow!("failed to read applied Recovery"))?;
    Ok(AppliedRecovery {
        recovery: applied_recovery,
        files_changed: entries.len(),
        already_applied: false,
    })
}

fn ensure_recovery_is_applicable(recovery: &Recovery) -> Result<()> {
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
    Ok(())
}

fn select_recovery_entries(
    project: &WatchedProject,
    entries: &[RecoveryEntry],
    paths: &[String],
) -> Result<Vec<RecoveryEntry>> {
    if paths.is_empty() {
        anyhow::bail!("select at least one file to restore");
    }

    let available = entries
        .iter()
        .map(|entry| {
            (
                crate::relative_path(&entry.path, &project.root_path).to_string(),
                entry,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut requested = BTreeSet::new();
    for path in paths {
        if path.trim().is_empty() {
            anyhow::bail!("selected restore path cannot be empty");
        }
        if !available.contains_key(path) {
            anyhow::bail!("selected path '{}' is not part of this recovery plan", path);
        }
        requested.insert(path.as_str());
    }

    Ok(available
        .into_iter()
        .filter(|(path, _)| requested.contains(path.as_str()))
        .map(|(_, entry)| (*entry).clone())
        .collect())
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

fn ensure_reported_paths_recoverable(
    db: &Database,
    project: &WatchedProject,
    base_dir: &std::path::Path,
    run: &Session,
    paths: &[String],
) -> Result<()> {
    if !run.is_reported() {
        return Ok(());
    }
    let mut blocked = Vec::new();
    for path in paths {
        let absolute = crate::safe_resolve_path(base_dir, path, &project.root_path)?;
        let absolute = absolute.to_string_lossy().into_owned();
        let status = db.classify_run_path_ownership(run.id, &absolute)?;
        if status != "exclusive" {
            blocked.push(format!(
                "{} ({})",
                crate::relative_path(&absolute, &project.root_path),
                status
            ));
        }
    }
    if !blocked.is_empty() {
        anyhow::bail!(
            "No recovery plan was created because reported ownership is not exclusive for: {}. \
             Collision, interleaved, and unattributed paths cannot be safely restored as whole files.",
            blocked.join(", ")
        );
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
    fn reported_recovery_blocks_collision_and_interleaving() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let path = root.join("src/main.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "current").unwrap();
        let absolute = path.to_string_lossy().into_owned();
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        let first = db
            .start_reported_run(
                project.id,
                "first",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:first-recovery",
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
                "codex:second-recovery",
            )
            .unwrap();
        db.open_run_boundary(first.id, "first-change", std::slice::from_ref(&absolute))
            .unwrap();
        db.open_run_boundary(second.id, "second-change", std::slice::from_ref(&absolute))
            .unwrap();
        db.insert_event(
            project.id,
            &absolute,
            "MODIFIED",
            Some("new"),
            Some("old"),
            None,
            None,
            Some(7),
        )
        .unwrap();
        db.close_run_boundary(first.id, "first-change", std::slice::from_ref(&absolute))
            .unwrap();
        db.close_run_boundary(second.id, "second-change", std::slice::from_ref(&absolute))
            .unwrap();
        let selected = vec!["src/main.rs".to_string()];
        let collision = ensure_reported_paths_recoverable(&db, &project, &root, &first, &selected)
            .unwrap_err()
            .to_string();
        assert!(collision.contains("collision"), "{collision}");

        let other = root.join("src/other.rs").to_string_lossy().into_owned();
        std::fs::write(&other, "current").unwrap();
        db.open_run_boundary(first.id, "exclusive", std::slice::from_ref(&other))
            .unwrap();
        db.insert_event(
            project.id,
            &other,
            "MODIFIED",
            Some("run"),
            Some("old"),
            None,
            None,
            Some(7),
        )
        .unwrap();
        db.close_run_boundary(first.id, "exclusive", std::slice::from_ref(&other))
            .unwrap();
        db.insert_event(
            project.id,
            &other,
            "MODIFIED",
            Some("later"),
            Some("run"),
            None,
            None,
            Some(7),
        )
        .unwrap();
        let interleaved = ensure_reported_paths_recoverable(
            &db,
            &project,
            &root,
            &first,
            &["src/other.rs".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(interleaved.contains("interleaved"), "{interleaved}");
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
    fn selected_apply_uses_derived_plan_and_preserves_source() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        let entries = [
            RecoveryEntry {
                recovery_id: 0,
                path: first.to_string_lossy().to_string(),
                action: "DELETE".to_string(),
                target_hash: None,
                source_timestamp: None,
                expected_hash: Some(snapshots::hash_bytes(b"first")),
                expected_exists: true,
            },
            RecoveryEntry {
                recovery_id: 0,
                path: second.to_string_lossy().to_string(),
                action: "DELETE".to_string(),
                target_hash: None,
                source_timestamp: None,
                expected_hash: Some(snapshots::hash_bytes(b"second")),
                expected_exists: true,
            },
        ];
        let source = db
            .create_recovery(
                project.id,
                None,
                "restore preview",
                "timestamp",
                "exact-timestamp",
                None,
                &entries,
            )
            .unwrap();

        let selected = vec!["first.txt".to_string()];
        let outcome =
            apply_recovery_paths_in(&db, &project, &source.public_id(), Some(&selected)).unwrap();

        assert_eq!(outcome.files_changed, 1);
        assert_ne!(outcome.recovery.id, source.id);
        assert_eq!(outcome.recovery.status, "applied");
        assert!(!first.exists());
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        let unchanged_source = db
            .get_recovery_by_ref(project.id, &source.public_id())
            .unwrap()
            .unwrap();
        assert_eq!(unchanged_source.status, "planned");
        assert_eq!(
            db.get_recovery_entries(outcome.recovery.id).unwrap().len(),
            1
        );
    }

    #[test]
    fn selected_apply_rejects_empty_and_unknown_paths() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("kept.txt");
        std::fs::write(&file, "kept").unwrap();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        let source = db
            .create_recovery(
                project.id,
                None,
                "restore preview",
                "timestamp",
                "exact-timestamp",
                None,
                &[RecoveryEntry {
                    recovery_id: 0,
                    path: file.to_string_lossy().to_string(),
                    action: "DELETE".to_string(),
                    target_hash: None,
                    source_timestamp: None,
                    expected_hash: Some(snapshots::hash_bytes(b"kept")),
                    expected_exists: true,
                }],
            )
            .unwrap();

        let empty = Vec::new();
        let error = apply_recovery_paths_in(&db, &project, &source.public_id(), Some(&empty))
            .err()
            .unwrap();
        assert!(error.to_string().contains("select at least one file"));

        let unknown = vec!["unknown.txt".to_string()];
        let error = apply_recovery_paths_in(&db, &project, &source.public_id(), Some(&unknown))
            .err()
            .unwrap();
        assert!(error.to_string().contains("not part of this recovery plan"));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "kept");
        let unchanged_source = db
            .get_recovery_by_ref(project.id, &source.public_id())
            .unwrap()
            .unwrap();
        assert_eq!(unchanged_source.status, "planned");
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
