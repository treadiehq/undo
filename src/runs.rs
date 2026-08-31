use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Utc};
use serde_json::json;
use std::path::Path;

use crate::db::Database;
use crate::models::{Session, WatchedProject};
use crate::{
    BOLD, DIM, GREEN, RED, RESET, YELLOW, find_project, groups, project_for_run, resolve_project,
};

#[derive(Clone, Copy)]
pub enum Output {
    Text,
    Json,
    Silent,
}

#[derive(Default)]
pub struct StartRunOptions<'a> {
    pub name: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub command: Option<&'a str>,
    pub intent: Option<&'a str>,
    pub external_id: Option<&'a str>,
}

struct StartedRun {
    db: Database,
    project: WatchedProject,
    root: std::path::PathBuf,
    run: Session,
}

impl StartedRun {
    fn complete(&self, status: &str, output: Output, sync_files: bool) -> Result<Session> {
        if sync_files {
            crate::daemon::ensure_recording_for_path(&self.root)?;
            sync_project(&self.db, &self.project, &self.root)?;
        }
        let run = self.db.complete_run(self.run.id, status)?;
        print_completed(&run, output);
        Ok(run)
    }
}

pub fn cmd_run_start(options: StartRunOptions<'_>, output: Output) -> Result<Session> {
    Ok(start_run(options, output)?.run)
}

pub fn cmd_reported_run_start(options: StartRunOptions<'_>, output: Output) -> Result<Session> {
    Ok(start_run_with_mode(options, output, "reported")?.run)
}

fn start_run(options: StartRunOptions<'_>, output: Output) -> Result<StartedRun> {
    start_run_with_mode(options, output, "window")
}

fn start_run_with_mode(
    options: StartRunOptions<'_>,
    output: Output,
    attribution_mode: &str,
) -> Result<StartedRun> {
    let (db, project, root) = prepare_project_boundary()?;
    let actor = options.actor.unwrap_or(if options.agent.is_some() {
        "agent"
    } else {
        "human"
    });
    validate_actor(actor)?;
    let name = options
        .name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| generated_run_name(options.agent.unwrap_or(actor)));
    let run = if attribution_mode == "reported" {
        let external_id = options
            .external_id
            .ok_or_else(|| anyhow::anyhow!("a reported Run requires a stable external Run ID"))?;
        db.start_reported_run(
            project.id,
            &name,
            "run",
            actor,
            options.agent,
            options.command,
            options.intent,
            external_id,
        )?
    } else {
        db.start_run(
            project.id,
            &name,
            "run",
            actor,
            options.agent,
            options.command,
            options.intent,
            options.external_id,
        )?
    };
    print_started(&run, output, &root)?;
    Ok(StartedRun {
        db,
        project,
        root,
        run,
    })
}

pub fn cmd_run_stop(reference: Option<&str>, status: &str, output: Output) -> Result<Session> {
    validate_completion_status(status)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let root = crate::daemon::ensure_recording_for_path(&cwd)?;
    let db = Database::open()?;
    let project = find_project(&db, &root)?;
    sync_project(&db, &project, &root)?;
    let run = active_run_for_context(&db, &cwd, project.id, reference)?.ok_or_else(|| {
        reference.map_or_else(
            || anyhow::anyhow!("No active Run."),
            |reference| anyhow::anyhow!("Run '{}' not found", reference),
        )
    })?;
    let run = db.complete_run(run.id, status)?;
    print_completed(&run, output);
    Ok(run)
}

pub fn cmd_runs(output: Output) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let runs = list_runs_for_context(&db, &project, &cwd)?;
    if matches!(output, Output::Json) {
        let rows = runs
            .iter()
            .map(|run| {
                json!({
                    "run_id": run.public_id(),
                    "run": run,
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }
    if runs.is_empty() {
        println!("No recorded Runs yet.");
        println!("Start an agent with: undo run claude");
        return Ok(());
    }
    println!("{}Recorded Runs{}", BOLD, RESET);
    println!();
    println!("ID        WHO            ELAPSED    CHANGES  STATUS");
    for run in runs {
        let events = db.get_session_events(&run)?;
        println!(
            "{:<9} {:<14} {:<10} {:<8} {}",
            run.public_id(),
            actor_label(&run),
            crate::duration::format_elapsed(
                run.ended_at
                    .unwrap_or_else(|| Utc::now().timestamp())
                    .saturating_sub(run.started_at)
            ),
            events.len(),
            status_label(&run.status)
        );
    }
    Ok(())
}

pub fn cmd_run_show(reference: &str, output: Output) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let resolved_project = resolve_project(&db, &cwd)?;
    let project_ids = run_context_project_ids(&db, &cwd, resolved_project.id)?;
    let run = db
        .get_historical_run_by_ref(&project_ids, reference)?
        .ok_or_else(|| anyhow::anyhow!("Run '{}' not found", reference))?;
    let project = project_for_run(&db, &run)?;
    let events = db.get_session_events(&run)?;
    let intents = db.list_run_intents(run.id)?;
    let checkpoints = db
        .list_checkpoints(project.id)?
        .into_iter()
        .filter(|checkpoint| checkpoint.run_id == Some(run.id))
        .collect::<Vec<_>>();
    let change_groups = groups::build_groups(&project, &events);

    if matches!(output, Output::Json) {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "run_id": run.public_id(),
                "run": run,
                "changes": events,
                "intents": intents,
                "checkpoints": checkpoints,
            }))?
        );
        return Ok(());
    }

    println!("{}Run {}{} — {}", BOLD, run.public_id(), RESET, run.name);
    println!("Actor:       {}", actor_label(&run));
    println!("Status:      {}", status_label(&run.status));
    println!("Started:     {}", format_local_time(run.started_at));
    if let Some(ended_at) = run.ended_at {
        println!("Finished:    {}", format_local_time(ended_at));
    }
    println!("Duration:    {}", run_duration(&run));
    println!("File changes: {}", events.len());
    if let Some(command) = &run.command {
        println!("Command:     {}", command);
    }
    if let Some(intent) = &run.intent {
        println!("Note:        {}", intent);
    }

    let mut chronology = Vec::new();
    chronology.push((
        run.start_event_id,
        0_u8,
        format!("Run started — {}", actor_label(&run)),
    ));
    for event in &events {
        chronology.push((
            event.id,
            1,
            format!(
                "{} {}",
                file_change_label(&event.event_type),
                crate::relative_path(&event.path, &project.root_path)
            ),
        ));
    }
    for checkpoint in &checkpoints {
        if let Some(event_id) = checkpoint.event_id {
            chronology.push((
                event_id,
                2,
                format!(
                    "Checkpoint {} ({})",
                    checkpoint.name,
                    checkpoint.public_id()
                ),
            ));
        }
    }
    for intent in &intents {
        chronology.push((
            intent.start_event_id,
            2,
            format!("Task started {} ({})", intent.label, intent.public_id()),
        ));
        if let Some(event_id) = intent.end_event_id {
            chronology.push((
                event_id,
                3,
                format!("Task finished {} ({})", intent.label, intent.public_id()),
            ));
        }
    }
    if let Some(event_id) = run.end_event_id {
        chronology.push((
            event_id,
            4,
            format!("Run finished — {}", status_label(&run.status)),
        ));
    }
    chronology.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    println!();
    println!("{}History{}", BOLD, RESET);
    for (event_id, _, label) in chronology {
        println!("  {} (change {})", label, event_id);
    }

    if !checkpoints.is_empty() {
        println!();
        println!("{}Checkpoints{}", BOLD, RESET);
        for checkpoint in checkpoints {
            println!(
                "  {:<8} {}{}{}{}",
                checkpoint.public_id(),
                checkpoint.name,
                checkpoint
                    .intent
                    .as_deref()
                    .map(|intent| format!(" — {}", intent))
                    .unwrap_or_default(),
                checkpoint
                    .event_id
                    .map(|event_id| format!(" (change {})", event_id))
                    .unwrap_or_else(|| " (saved by time)".to_string()),
                RESET
            );
        }
    }
    if !intents.is_empty() {
        println!();
        println!("{}Task boundaries{}", BOLD, RESET);
        for intent in intents {
            println!(
                "  {:<8} {} ({})",
                intent.public_id(),
                intent.label,
                intent.status
            );
        }
    }
    if !change_groups.is_empty() {
        println!();
        println!("{}File groups{}", BOLD, RESET);
        for group in change_groups {
            println!(
                "  {:<18} {} files, {} recorded changes, +{} -{}",
                group.label,
                group.paths.len(),
                group.event_count,
                group.inserted,
                group.deleted
            );
        }
    }
    Ok(())
}

/// List Runs from every watched project on the current directory's ancestor
/// chain. This remains stable if recording moves from parent to nested or vice
/// versa, without pulling in sibling project history (#85, #88).
fn list_runs_for_context(
    db: &Database,
    recording_project: &WatchedProject,
    cwd: &Path,
) -> Result<Vec<Session>> {
    let mut runs = Vec::new();
    for project_id in run_context_project_ids(db, cwd, recording_project.id)? {
        runs.extend(db.list_sessions(project_id)?);
    }
    runs.sort_by(|left, right| {
        right
            .started_at
            .cmp(&left.started_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(runs)
}

/// Durable project context for Run history: every watched root containing cwd,
/// independent of which daemon happens to be active now. The currently
/// resolved project is included first as a defensive fallback (#85, #88).
pub(crate) fn run_context_project_ids(
    db: &Database,
    cwd: &Path,
    resolved_project_id: i64,
) -> Result<Vec<i64>> {
    let mut ids = vec![resolved_project_id];
    for project in db.list_projects_for_path(cwd)? {
        if !ids.contains(&project.id) {
            ids.push(project.id);
        }
    }
    Ok(ids)
}

/// Find a Run within the cwd's durable project context. Unlike the historical
/// public-id lookup, this never crosses into an unrelated project, so it is
/// safe for state-changing operations such as `run stop`.
pub(crate) fn find_run_in_context(
    db: &Database,
    project_ids: &[i64],
    reference: &str,
) -> Result<Option<Session>> {
    let mut matches = Vec::new();
    for project_id in project_ids {
        if let Some(run) = db.get_run_by_ref(*project_id, reference)?
            && !matches
                .iter()
                .any(|existing: &Session| existing.id == run.id)
        {
            matches.push(run);
        }
    }
    match matches.as_slice() {
        [] => Ok(None),
        [run] => Ok(Some(run.clone())),
        runs => {
            let ids = runs
                .iter()
                .map(Session::public_id)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "Run name '{}' is ambiguous across projects ({ids}); use an r_<id> reference",
                reference
            )
        }
    }
}

/// Resolve the active Run to close across every project root containing cwd.
/// A topology change may switch the daemon-selected project between start and
/// stop, but the durable watched-project chain remains stable. Multiple active
/// matches are never guessed; the caller must provide an explicit Run id.
pub(crate) fn active_run_for_context(
    db: &Database,
    cwd: &Path,
    resolved_project_id: i64,
    reference: Option<&str>,
) -> Result<Option<Session>> {
    let project_ids = run_context_project_ids(db, cwd, resolved_project_id)?;
    if let Some(reference) = reference {
        // Preserve `complete_run`'s idempotency for an explicitly referenced
        // already-complete Run; only the no-reference path requires active rows.
        return find_run_in_context(db, &project_ids, reference);
    }

    let mut active = Vec::new();
    for project_id in project_ids {
        active.extend(db.list_active_runs(project_id)?);
    }
    active.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    match active.as_slice() {
        [] => Ok(None),
        [run] => Ok(Some(run.clone())),
        runs => {
            let ids = runs
                .iter()
                .map(Session::public_id)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "multiple Runs are active across this folder's project history ({ids}); \
                 specify one with `undo run stop <RUN>`"
            )
        }
    }
}

pub fn cmd_run_command(
    command: &[String],
    agent_override: Option<&str>,
    name: Option<&str>,
    intent: Option<&str>,
) -> Result<i32> {
    let executable = command.first().ok_or_else(|| {
        anyhow::anyhow!("No command was provided.\nPass a command after `undo run`.")
    })?;
    let inferred_agent = agent_override
        .map(str::to_string)
        .or_else(|| infer_agent(executable));
    let command_display = command.join(" ");
    let started = start_run(
        StartRunOptions {
            name,
            actor: Some(if inferred_agent.is_some() {
                "agent"
            } else {
                "tool"
            }),
            agent: inferred_agent.as_deref(),
            command: Some(&command_display),
            intent,
            external_id: None,
        },
        Output::Text,
    )?;

    let result = std::process::Command::new(executable)
        .args(&command[1..])
        .status()
        .with_context(|| format!("failed to launch '{}'", executable));
    let (run_status, exit_code) = match result {
        Ok(status) if status.success() => ("completed", status.code().unwrap_or(0)),
        Ok(status) => ("failed", status.code().unwrap_or(1)),
        Err(error) => {
            return complete_failed_launch(&started, error);
        }
    };
    started.complete(run_status, Output::Text, true)?;
    Ok(exit_code)
}

fn complete_failed_launch(started: &StartedRun, launch_error: anyhow::Error) -> Result<i32> {
    if let Err(cleanup_error) = started.complete("failed", Output::Text, false) {
        let run_id = started.run.public_id();
        anyhow::bail!(
            "{}\nUndo also could not mark Run {} as failed: {}\n\
             Complete it manually with: undo run stop {} --status failed",
            launch_error,
            run_id,
            cleanup_error,
            run_id
        );
    }
    Err(launch_error)
}

pub fn cmd_run_shorthand(mut command: Vec<String>) -> Result<i32> {
    normalize_shorthand_command(&mut command);
    cmd_run_command(&command, None, None, None)
}

fn normalize_shorthand_command(command: &mut Vec<String>) {
    if let Some(separator) = command.iter().position(|argument| argument == "--") {
        command.remove(separator);
    }
}

pub fn prepare_project_boundary() -> Result<(Database, WatchedProject, std::path::PathBuf)> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let root = crate::daemon::ensure_recording_for_path(&cwd)?;
    let db = Database::open()?;
    let project = find_project(&db, &root)?;
    sync_project(&db, &project, &root)?;
    Ok((db, project, root))
}

pub fn sync_project(db: &Database, project: &WatchedProject, root: &Path) -> Result<()> {
    crate::ignore::init(root);
    crate::watcher::initial_scan(db, project, root, false, true)
}

fn generated_run_name(actor: &str) -> String {
    let slug = actor
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    format!(
        "{}-{}-{}",
        if slug.is_empty() {
            "run"
        } else {
            slug.as_str()
        },
        Utc::now().timestamp(),
        std::process::id()
    )
}

fn infer_agent(executable: &str) -> Option<String> {
    let basename = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable)
        .to_ascii_lowercase();
    match basename.as_str() {
        "claude" | "claude-code" => Some("Claude Code".to_string()),
        "opencode" => Some("OpenCode".to_string()),
        "codex" => Some("Codex".to_string()),
        _ => None,
    }
}

fn validate_actor(actor: &str) -> Result<()> {
    if matches!(actor, "human" | "agent" | "tool" | "mixed") {
        Ok(())
    } else {
        anyhow::bail!("actor must be one of: human, agent, tool, mixed")
    }
}

fn validate_completion_status(status: &str) -> Result<()> {
    if matches!(status, "completed" | "failed" | "aborted") {
        Ok(())
    } else {
        anyhow::bail!("Run status must be completed, failed, or aborted")
    }
}

fn actor_label(run: &Session) -> String {
    run.agent
        .clone()
        .unwrap_or_else(|| match run.actor.as_str() {
            "human" => "Human".to_string(),
            "tool" => "Tool".to_string(),
            "mixed" => "Mixed".to_string(),
            other => other.to_string(),
        })
}

fn status_label(status: &str) -> &str {
    match status {
        "active" => "Active",
        "completed" => "Completed",
        "failed" => "Failed",
        "aborted" => "Aborted",
        other => other,
    }
}

fn file_change_label(event_type: &str) -> &str {
    match event_type {
        "MODIFIED" => "Modified",
        "CREATED" => "Created",
        "DELETED" => "Deleted",
        "RENAMED" => "Renamed",
        other => other,
    }
}

fn run_outcome_heading(status: &str) -> &str {
    match status {
        "completed" => "Run completed",
        "failed" => "Run failed",
        "aborted" => "Run aborted",
        _ => "Run finished",
    }
}

fn run_duration(run: &Session) -> String {
    crate::duration::format_elapsed(
        run.ended_at
            .unwrap_or_else(|| Utc::now().timestamp())
            .saturating_sub(run.started_at),
    )
}

fn format_local_time(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

fn print_started(run: &Session, output: Output, root: &Path) -> Result<()> {
    if matches!(output, Output::Silent) {
        return Ok(());
    }
    if matches!(output, Output::Json) {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "event": "run_started",
                "run": run,
                "run_id": run.public_id(),
                "project": root,
            }))?
        );
    } else {
        println!(
            "{}Recording started for Run {}{} ({}, {}).",
            GREEN,
            run.public_id(),
            RESET,
            actor_label(run),
            run.name
        );
        println!("Folder: {}", root.display());
    }
    Ok(())
}

fn print_completed(run: &Session, output: Output) {
    if matches!(output, Output::Silent) {
        return;
    }
    if matches!(output, Output::Json) {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "event": "run_completed",
                "run": run,
                "run_id": run.public_id(),
            }))
            .unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        let color = match run.status.as_str() {
            "completed" => GREEN,
            "failed" => RED,
            "aborted" => YELLOW,
            _ => RESET,
        };
        println!(
            "{}{}{} {} ({}).",
            color,
            run_outcome_heading(&run.status),
            RESET,
            run.public_id(),
            run_duration(run)
        );
        if run.status != "completed" {
            println!(
                "Inspect its recorded file changes with: undo run show {}",
                run.public_id()
            );
        }
        // The moment a Run ends is when review matters; point at the focused
        // browser view instead of making the user reconstruct it later.
        println!(
            "{}Review it in the browser: undo ui {}{}",
            DIM,
            run.public_id(),
            RESET
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// With a parent project currently recording, `run list` includes Runs from
    /// historical nested projects on the cwd path, but not sibling projects
    /// under the same parent (#85).
    #[test]
    fn run_list_context_includes_nested_history_but_not_siblings() {
        let db = Database::open_in_memory().unwrap();
        let parent = db.get_or_create_project(Path::new("/work")).unwrap();
        let nested = db
            .get_or_create_project(Path::new("/work/packages/app"))
            .unwrap();
        let sibling = db
            .get_or_create_project(Path::new("/work/packages/other"))
            .unwrap();

        let parent_run = db
            .start_run(parent.id, "parent", "run", "human", None, None, None, None)
            .unwrap();
        let nested_run = db
            .start_run(nested.id, "nested", "run", "human", None, None, None, None)
            .unwrap();
        db.start_run(
            sibling.id, "sibling", "run", "human", None, None, None, None,
        )
        .unwrap();

        for resolved in [&parent, &nested] {
            let runs =
                list_runs_for_context(&db, resolved, Path::new("/work/packages/app/src/main.rs"))
                    .unwrap();
            let ids = runs.iter().map(|run| run.id).collect::<BTreeSet<_>>();
            assert_eq!(
                ids,
                BTreeSet::from([parent_run.id, nested_run.id]),
                "list output must not change when daemon topology flips"
            );
        }
    }

    /// Stopping without an explicit id finds the sole active Run anywhere on
    /// the cwd's watched-project ancestor chain, regardless of which project
    /// the current daemon resolves to (#88).
    #[test]
    fn active_run_context_survives_parent_nested_topology_flips() {
        let db = Database::open_in_memory().unwrap();
        let parent = db.get_or_create_project(Path::new("/work")).unwrap();
        let nested = db
            .get_or_create_project(Path::new("/work/packages/app"))
            .unwrap();
        let cwd = Path::new("/work/packages/app/src");

        let nested_run = db
            .start_run(nested.id, "nested", "run", "human", None, None, None, None)
            .unwrap();
        let found = active_run_for_context(&db, cwd, parent.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(found.id, nested_run.id);
        db.complete_run(nested_run.id, "completed").unwrap();

        let parent_run = db
            .start_run(parent.id, "parent", "run", "human", None, None, None, None)
            .unwrap();
        let found = active_run_for_context(&db, cwd, nested.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(found.id, parent_run.id);
    }

    /// If both parent and nested projects have active Runs, stop must never
    /// guess. The error gives stable ids, and an explicit id selects only the
    /// requested Run within the cwd context (#88).
    #[test]
    fn active_run_context_requires_id_when_multiple_runs_are_active() {
        let db = Database::open_in_memory().unwrap();
        let parent = db.get_or_create_project(Path::new("/work")).unwrap();
        let nested = db
            .get_or_create_project(Path::new("/work/packages/app"))
            .unwrap();
        let sibling = db
            .get_or_create_project(Path::new("/work/packages/other"))
            .unwrap();
        let cwd = Path::new("/work/packages/app/src");
        let parent_run = db
            .start_run(parent.id, "same", "run", "human", None, None, None, None)
            .unwrap();
        let nested_run = db
            .start_run(nested.id, "same", "run", "human", None, None, None, None)
            .unwrap();
        let sibling_run = db
            .start_run(
                sibling.id, "sibling", "run", "human", None, None, None, None,
            )
            .unwrap();

        let error = active_run_for_context(&db, cwd, parent.id, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&parent_run.public_id()), "{error}");
        assert!(error.contains(&nested_run.public_id()), "{error}");

        let selected = active_run_for_context(&db, cwd, parent.id, Some(&nested_run.public_id()))
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, nested_run.id);

        let name_error = active_run_for_context(&db, cwd, parent.id, Some("same"))
            .unwrap_err()
            .to_string();
        assert!(name_error.contains("ambiguous"), "{name_error}");
        assert!(
            active_run_for_context(&db, cwd, parent.id, Some(&sibling_run.public_id()))
                .unwrap()
                .is_none(),
            "state-changing lookup must not cross into a sibling project"
        );
    }

    #[test]
    fn known_agent_commands_are_identified() {
        assert_eq!(
            infer_agent("/usr/local/bin/claude").as_deref(),
            Some("Claude Code")
        );
        assert_eq!(infer_agent("opencode").as_deref(), Some("OpenCode"));
        assert_eq!(infer_agent("codex").as_deref(), Some("Codex"));
        assert_eq!(infer_agent("cargo"), None);
    }

    #[test]
    fn generated_names_are_nonempty_and_agent_scoped() {
        assert!(generated_run_name("Claude Code").starts_with("claude-code-"));
    }

    #[test]
    fn shorthand_removes_only_its_first_separator() {
        let mut command = vec![
            "cargo".to_string(),
            "--".to_string(),
            "test".to_string(),
            "--".to_string(),
            "--nocapture".to_string(),
        ];
        normalize_shorthand_command(&mut command);
        assert_eq!(command, vec!["cargo", "test", "--", "--nocapture"]);
    }

    #[test]
    fn failed_launch_completion_clears_the_active_run() {
        let db = Database::open_in_memory().unwrap();
        let cwd = std::path::PathBuf::from("/project");
        let project = db.get_or_create_project(&cwd).unwrap();
        let run = db
            .start_run(
                project.id,
                "missing-command",
                "run",
                "tool",
                None,
                Some("definitely-not-an-executable"),
                None,
                None,
            )
            .unwrap();
        let run_id = run.id;
        let started = StartedRun {
            db,
            project,
            root: cwd,
            run,
        };

        let error = complete_failed_launch(
            &started,
            anyhow::anyhow!("failed to launch 'definitely-not-an-executable'"),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "failed to launch 'definitely-not-an-executable'"
        );
        assert!(
            started
                .db
                .get_active_session(started.project.id)
                .unwrap()
                .is_none()
        );
        let completed = started.db.get_session_by_id(run_id).unwrap().unwrap();
        assert_eq!(completed.status, "failed");
        assert!(completed.ended_at.is_some());
    }

    #[test]
    fn run_outcomes_match_the_stored_status() {
        assert_eq!(run_outcome_heading("completed"), "Run completed");
        assert_eq!(run_outcome_heading("failed"), "Run failed");
        assert_eq!(run_outcome_heading("aborted"), "Run aborted");
        assert_eq!(run_outcome_heading("active"), "Run finished");
    }

    #[test]
    fn stored_status_and_event_values_are_humanized_only_for_display() {
        assert_eq!(status_label("active"), "Active");
        assert_eq!(status_label("failed"), "Failed");
        assert_eq!(file_change_label("MODIFIED"), "Modified");
        assert_eq!(file_change_label("CUSTOM"), "CUSTOM");
    }
}
