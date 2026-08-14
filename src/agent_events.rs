use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use crate::db::Database;
use crate::models::{Session, WatchedProject};
use crate::runs::{self, Output, StartRunOptions};

pub const AGENT_EVENT_VERSION: u32 = 1;
pub const REPORTED_AGENT_EVENT_VERSION: u32 = 2;
const MAX_REPORTED_PATHS: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentEvent {
    version: u32,
    event: String,
    idempotency_key: String,
    run_id: Option<String>,
    name: Option<String>,
    actor: Option<String>,
    agent: Option<String>,
    command: Option<String>,
    intent: Option<String>,
    status: Option<String>,
    external_run_id: Option<String>,
    external_change_id: Option<String>,
    paths: Option<Vec<String>>,
}

pub fn cmd_event(inline: Option<&str>) -> Result<()> {
    let payload = match inline {
        Some(payload) => payload.to_string(),
        None => {
            let mut payload = String::new();
            std::io::stdin().read_to_string(&mut payload)?;
            payload
        }
    };
    if payload.trim().is_empty() {
        anyhow::bail!(
            "event requires one JSON object on stdin or through --json.\n\
             Example: printf '%s' '{{\"version\":1,\"event\":\"run_started\",\
             \"idempotency_key\":\"agent-42-start\",\"agent\":\"Claude Code\"}}' | undo event"
        );
    }
    let event: AgentEvent = serde_json::from_str(&payload)
        .map_err(|error| anyhow::anyhow!("Invalid JSON: {}", error))?;
    validate_event(&event)?;
    let request_hash = request_hash(&payload)?;

    let db = Database::open()?;
    if let Some(response) = db.get_integration_response(&event.idempotency_key, &request_hash)? {
        println!("{}", response);
        return Ok(());
    }

    let (run_id, response) = process_event(&event)?;
    let response_json = serde_json::to_string(&response)?;
    let db = Database::open()?;
    if let Some(existing) = db.get_integration_response(&event.idempotency_key, &request_hash)? {
        println!("{}", existing);
        return Ok(());
    }
    if let Err(error) = db.record_integration_response(
        &event.idempotency_key,
        &request_hash,
        run_id,
        &event.event,
        &response_json,
    ) {
        if let Some(existing) =
            db.get_integration_response(&event.idempotency_key, &request_hash)?
        {
            println!("{}", existing);
            return Ok(());
        }
        return Err(error);
    }
    println!("{}", response_json);
    Ok(())
}

fn request_hash(payload: &str) -> Result<String> {
    // Hash canonical JSON rather than raw whitespace so semantically identical
    // retries replay, while reusing a key for a changed payload is rejected.
    let value: Value = serde_json::from_str(payload)?;
    let canonical = serde_json::to_vec(&value)?;
    Ok(crate::to_hex(&Sha256::digest(canonical)))
}

fn process_event(event: &AgentEvent) -> Result<(Option<i64>, Value)> {
    match event.event.as_str() {
        "run_started" => {
            let external_id = event
                .external_run_id
                .as_deref()
                .unwrap_or(&event.idempotency_key);
            let options = StartRunOptions {
                name: event.name.as_deref(),
                actor: event.actor.as_deref().or(Some("agent")),
                agent: event.agent.as_deref(),
                command: event.command.as_deref(),
                intent: event.intent.as_deref(),
                external_id: Some(external_id),
            };
            let run = if event.version == REPORTED_AGENT_EVENT_VERSION {
                runs::cmd_reported_run_start(options, Output::Silent)?
            } else {
                runs::cmd_run_start(options, Output::Silent)?
            };
            Ok((
                Some(run.id),
                json!({
                    "version": event.version,
                    "event": "run_started",
                    "run_id": run.public_id(),
                    "status": run.status,
                }),
            ))
        }
        "change_started" => {
            let (db, project, root) = runs::prepare_project_boundary()?;
            let run = required_reported_run(&db, project.id, event)?;
            let paths = normalize_reported_paths(
                &project,
                &root,
                event.paths.as_deref().expect("validated paths"),
            )?;
            let external_change_id = event
                .external_change_id
                .as_deref()
                .expect("validated change id");
            let boundary = db.open_run_boundary(run.id, external_change_id, &paths)?;
            Ok((
                Some(run.id),
                json!({
                    "version": event.version,
                    "event": "change_started",
                    "run_id": run.public_id(),
                    "external_run_id": event.external_run_id,
                    "external_change_id": external_change_id,
                    "status": boundary.status,
                }),
            ))
        }
        "change_completed" => {
            let (db, project, root) = runs::prepare_project_boundary()?;
            let run = required_reported_run(&db, project.id, event)?;
            let paths = normalize_reported_paths(
                &project,
                &root,
                event.paths.as_deref().expect("validated paths"),
            )?;
            let external_change_id = event
                .external_change_id
                .as_deref()
                .expect("validated change id");
            let boundary = db.close_run_boundary(run.id, external_change_id, &paths)?;
            Ok((
                Some(run.id),
                json!({
                    "version": event.version,
                    "event": "change_completed",
                    "run_id": run.public_id(),
                    "external_run_id": event.external_run_id,
                    "external_change_id": external_change_id,
                    "status": boundary.status,
                    "claimed_events": db.count_run_claimed_events(run.id)?,
                }),
            ))
        }
        "checkpoint" => {
            let (db, project, _) = runs::prepare_project_boundary()?;
            let run = if event.version == REPORTED_AGENT_EVENT_VERSION {
                required_reported_run(&db, project.id, event)?
            } else {
                required_active_run(&db, project.id, event.run_id.as_deref())?
            };
            let name = required_checkpoint_name(event)?;
            let (checkpoint, created) =
                db.create_checkpoint_now(project.id, Some(run.id), name, event.intent.as_deref())?;
            Ok((
                Some(run.id),
                json!({
                    "version": event.version,
                    "event": "checkpoint",
                    "run_id": run.public_id(),
                    "checkpoint_id": checkpoint.public_id(),
                    "created": created,
                }),
            ))
        }
        "intent_started" => {
            let (db, project, _) = runs::prepare_project_boundary()?;
            let run = if event.version == REPORTED_AGENT_EVENT_VERSION {
                required_reported_run(&db, project.id, event)?
            } else {
                required_active_run(&db, project.id, event.run_id.as_deref())?
            };
            let label = required_intent(event)?;
            let intent = db.start_run_intent(run.id, label)?;
            Ok((
                Some(run.id),
                json!({
                    "version": event.version,
                    "event": "intent_started",
                    "run_id": run.public_id(),
                    "intent_id": intent.public_id(),
                    "intent": intent.label,
                }),
            ))
        }
        "intent_completed" => {
            let (db, project, _) = runs::prepare_project_boundary()?;
            let run = if event.version == REPORTED_AGENT_EVENT_VERSION {
                required_reported_run(&db, project.id, event)?
            } else {
                required_active_run(&db, project.id, event.run_id.as_deref())?
            };
            let intent = db.complete_run_intent(run.id, event.intent.as_deref())?;
            Ok((
                Some(run.id),
                json!({
                    "version": event.version,
                    "event": "intent_completed",
                    "run_id": run.public_id(),
                    "intent_id": intent.public_id(),
                    "intent": intent.label,
                }),
            ))
        }
        "run_completed" => {
            let run = if event.version == REPORTED_AGENT_EVENT_VERSION {
                let (db, project, _) = runs::prepare_project_boundary()?;
                let external_id = event
                    .external_run_id
                    .as_deref()
                    .expect("v2 validation requires external_run_id");
                let run = db
                    .get_run_by_external_id(project.id, external_id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "reported Run '{}' not found; send v2 run_started first",
                            external_id
                        )
                    })?;
                if !run.is_reported() {
                    anyhow::bail!("Run {} does not use reported attribution", run.public_id());
                }
                if run.is_active() {
                    db.complete_run(run.id, event.status.as_deref().unwrap_or("completed"))?
                } else {
                    // A crash after the lifecycle mutation but before saving
                    // the idempotent response must still make retries succeed.
                    run
                }
            } else {
                runs::cmd_run_stop(
                    event.run_id.as_deref(),
                    event.status.as_deref().unwrap_or("completed"),
                    Output::Silent,
                )?
            };
            Ok((
                Some(run.id),
                json!({
                    "version": event.version,
                    "event": "run_completed",
                    "run_id": run.public_id(),
                    "status": run.status,
                }),
            ))
        }
        _ => unreachable!("validated event type"),
    }
}

fn required_reported_run(db: &Database, project_id: i64, event: &AgentEvent) -> Result<Session> {
    let external_id = event
        .external_run_id
        .as_deref()
        .expect("v2 validation requires external_run_id");
    let run = db
        .get_run_by_external_id(project_id, external_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "reported Run '{}' not found; send v2 run_started first",
                external_id
            )
        })?;
    if !run.is_reported() {
        anyhow::bail!("Run {} does not use reported attribution", run.public_id());
    }
    if !run.is_active() {
        anyhow::bail!("Run {} is already complete", run.public_id());
    }
    Ok(run)
}

fn normalize_reported_paths(
    project: &WatchedProject,
    root: &Path,
    paths: &[String],
) -> Result<Vec<String>> {
    if paths.is_empty() || paths.len() > MAX_REPORTED_PATHS {
        anyhow::bail!(
            "paths must contain 1–{} exact project files",
            MAX_REPORTED_PATHS
        );
    }
    let mut normalized = BTreeSet::new();
    for input in paths {
        let input = input.trim();
        if input.is_empty() {
            anyhow::bail!("reported paths cannot be empty");
        }
        if input.chars().any(|character| "*?[]{}".contains(character)) {
            anyhow::bail!(
                "reported path '{}' must be exact; globs are not allowed",
                input
            );
        }
        let resolved = crate::safe_resolve_path(root, input, &project.root_path)?;
        if resolved.is_dir() {
            anyhow::bail!(
                "reported path '{}' is a directory; exact file paths are required",
                input
            );
        }
        let absolute = resolved.to_string_lossy().into_owned();
        if !normalized.insert(absolute.clone()) {
            anyhow::bail!("reported paths contain duplicate '{}'", input);
        }
    }
    Ok(normalized.into_iter().collect())
}

fn required_active_run(
    db: &Database,
    project_id: i64,
    reference: Option<&str>,
) -> Result<crate::models::Session> {
    let run = match reference {
        Some(reference) => db
            .get_run_by_ref(project_id, reference)?
            .ok_or_else(|| anyhow::anyhow!("Run '{}' not found", reference))?,
        None => db
            .get_active_session(project_id)?
            .ok_or_else(|| anyhow::anyhow!("no active Run; send run_started first"))?,
    };
    if !run.is_active() {
        anyhow::bail!("Run {} is already complete", run.public_id());
    }
    Ok(run)
}

fn required_intent(event: &AgentEvent) -> Result<&str> {
    event
        .intent
        .as_deref()
        .map(str::trim)
        .filter(|intent| !intent.is_empty())
        .ok_or_else(|| anyhow::anyhow!("intent_started requires a non-empty 'intent'"))
}

fn required_checkpoint_name(event: &AgentEvent) -> Result<&str> {
    event
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("checkpoint event requires a non-empty 'name'"))
}

fn validate_event(event: &AgentEvent) -> Result<()> {
    if !matches!(
        event.version,
        AGENT_EVENT_VERSION | REPORTED_AGENT_EVENT_VERSION
    ) {
        anyhow::bail!(
            "unsupported event version {}; supported versions are {} and {}",
            event.version,
            AGENT_EVENT_VERSION,
            REPORTED_AGENT_EVENT_VERSION
        );
    }
    let supported = matches!(
        event.event.as_str(),
        "run_started" | "checkpoint" | "intent_started" | "intent_completed" | "run_completed"
    ) || (event.version == REPORTED_AGENT_EVENT_VERSION
        && matches!(event.event.as_str(), "change_started" | "change_completed"));
    if !supported {
        anyhow::bail!("unsupported v{} event '{}'", event.version, event.event);
    }
    if event.idempotency_key.trim().is_empty() || event.idempotency_key.len() > 200 {
        anyhow::bail!("idempotency_key must contain 1–200 UTF-8 bytes");
    }
    for (name, value, max) in [
        ("name", event.name.as_deref(), 200),
        ("actor", event.actor.as_deref(), 32),
        ("agent", event.agent.as_deref(), 200),
        ("command", event.command.as_deref(), 4096),
        ("intent", event.intent.as_deref(), 4096),
        ("status", event.status.as_deref(), 32),
        ("external_run_id", event.external_run_id.as_deref(), 200),
        (
            "external_change_id",
            event.external_change_id.as_deref(),
            200,
        ),
        ("run_id", event.run_id.as_deref(), 200),
    ] {
        if value.is_some_and(|value| value.len() > max) {
            anyhow::bail!("{} exceeds {} UTF-8 bytes", name, max);
        }
    }
    if event.event == "run_completed"
        && event
            .status
            .as_deref()
            .is_some_and(|status| !matches!(status, "completed" | "failed" | "aborted"))
    {
        anyhow::bail!("Run status must be completed, failed, or aborted");
    }
    if event.version == AGENT_EVENT_VERSION {
        if event.event != "run_started" && event.external_run_id.is_some() {
            anyhow::bail!("external_run_id is only valid for v1 run_started");
        }
        if event.external_change_id.is_some() || event.paths.is_some() {
            anyhow::bail!("external_change_id and paths require protocol version 2");
        }
        return Ok(());
    }

    let external_run_id = event
        .external_run_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("v2 events require a non-empty external_run_id"))?;
    let _ = external_run_id;
    if event.run_id.is_some() {
        anyhow::bail!("v2 resolves Runs by external_run_id; omit run_id");
    }
    let is_change = matches!(event.event.as_str(), "change_started" | "change_completed");
    if is_change {
        event
            .external_change_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("v2 change events require a non-empty external_change_id")
            })?;
        let paths = event
            .paths
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("v2 change events require non-empty exact paths"))?;
        if paths.is_empty() || paths.len() > MAX_REPORTED_PATHS {
            anyhow::bail!(
                "paths must contain 1–{} exact project files",
                MAX_REPORTED_PATHS
            );
        }
        if paths.iter().any(|path| path.len() > 4096) {
            anyhow::bail!("each reported path must be at most 4096 UTF-8 bytes");
        }
    } else if event.external_change_id.is_some() || event.paths.is_some() {
        anyhow::bail!("external_change_id and paths are only valid for v2 change events");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event: &str) -> AgentEvent {
        AgentEvent {
            version: 1,
            event: event.to_string(),
            idempotency_key: "test-1".to_string(),
            run_id: None,
            name: None,
            actor: None,
            agent: None,
            command: None,
            intent: None,
            status: None,
            external_run_id: None,
            external_change_id: None,
            paths: None,
        }
    }

    #[test]
    fn rejects_unknown_versions_and_events() {
        let mut wrong_version = event("run_started");
        wrong_version.version = 3;
        assert!(validate_event(&wrong_version).is_err());

        let unknown = event("file_changed");
        assert!(validate_event(&unknown).is_err());
    }

    #[test]
    fn strict_json_rejects_unknown_fields() {
        let result = serde_json::from_str::<AgentEvent>(
            r#"{"version":1,"event":"run_started","idempotency_key":"a","surprise":true}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn malformed_json_reports_json_error_directly() {
        let error = cmd_event(Some("{")).unwrap_err().to_string();

        assert!(error.starts_with("Invalid JSON:"), "{error}");
    }

    #[test]
    fn intent_started_requires_intent_text() {
        let missing = event("intent_started");
        assert!(required_intent(&missing).is_err());
    }

    #[test]
    fn validation_rejects_oversized_run_reference() {
        let mut oversized = event("checkpoint");
        oversized.run_id = Some("r".repeat(201));
        let error = validate_event(&oversized).unwrap_err().to_string();
        assert_eq!(error, "run_id exceeds 200 UTF-8 bytes");
    }

    #[test]
    fn string_limits_count_utf8_bytes() {
        let mut valid = event("run_started");
        valid.idempotency_key = "é".repeat(100);
        valid.name = Some("é".repeat(100));
        assert!(validate_event(&valid).is_ok());

        let mut oversized_key = event("run_started");
        oversized_key.idempotency_key = "é".repeat(101);
        assert_eq!(
            validate_event(&oversized_key).unwrap_err().to_string(),
            "idempotency_key must contain 1–200 UTF-8 bytes"
        );

        let mut oversized_name = event("run_started");
        oversized_name.name = Some("é".repeat(101));
        assert_eq!(
            validate_event(&oversized_name).unwrap_err().to_string(),
            "name exceeds 200 UTF-8 bytes"
        );
    }

    #[test]
    fn checkpoint_requires_meaningful_name() {
        let mut checkpoint = event("checkpoint");
        checkpoint.name = Some("   ".to_string());
        assert!(required_checkpoint_name(&checkpoint).is_err());
        checkpoint.name = Some("tests pass".to_string());
        assert_eq!(required_checkpoint_name(&checkpoint).unwrap(), "tests pass");
    }

    #[test]
    fn v2_requires_explicit_run_and_change_identity() {
        let mut started = event("run_started");
        started.version = 2;
        assert!(validate_event(&started).is_err());
        started.external_run_id = Some("conversation-1".to_string());
        assert!(validate_event(&started).is_ok());

        let mut change = event("change_started");
        change.version = 2;
        change.external_run_id = Some("conversation-1".to_string());
        assert!(validate_event(&change).is_err());
        change.external_change_id = Some("tool-1".to_string());
        change.paths = Some(vec!["src/main.rs".to_string()]);
        assert!(validate_event(&change).is_ok());
    }

    #[test]
    fn reported_path_validation_rejects_escape_glob_directory_and_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let project = WatchedProject {
            id: 1,
            root_path: root.to_string_lossy().into_owned(),
            created_at: 0,
        };
        assert!(normalize_reported_paths(&project, &root, &["../escape.rs".to_string()]).is_err());
        assert!(normalize_reported_paths(&project, &root, &["src/*.rs".to_string()]).is_err());
        assert!(normalize_reported_paths(&project, &root, &["src".to_string()]).is_err());
        assert!(
            normalize_reported_paths(
                &project,
                &root,
                &["src/main.rs".to_string(), "./src/main.rs".to_string()]
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_request_hash_ignores_json_whitespace() {
        assert_eq!(
            request_hash(r#"{"version":2,"event":"run_started"}"#).unwrap(),
            request_hash("{ \"event\": \"run_started\", \"version\": 2 }").unwrap()
        );
    }

    #[test]
    fn v2_resolution_uses_external_run_id_with_concurrent_runs() {
        let db = Database::open_in_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let project = db
            .get_or_create_project(&root.path().canonicalize().unwrap())
            .unwrap();
        let first = db
            .start_reported_run(
                project.id,
                "first",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:first",
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
                "codex:second",
            )
            .unwrap();
        let mut event = event("checkpoint");
        event.version = 2;
        event.external_run_id = Some("cursor:first".to_string());

        assert_eq!(
            required_reported_run(&db, project.id, &event).unwrap().id,
            first.id
        );
        assert_ne!(first.id, second.id);
    }
}
