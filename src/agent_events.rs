use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;

use crate::db::Database;
use crate::runs::{self, Output, StartRunOptions};

pub const AGENT_EVENT_VERSION: u32 = 1;

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

    let db = Database::open()?;
    if let Some(response) = db.get_integration_response(&event.idempotency_key)? {
        println!("{}", response);
        return Ok(());
    }

    let (run_id, response) = process_event(&event)?;
    let response_json = serde_json::to_string(&response)?;
    let db = Database::open()?;
    if let Some(existing) = db.get_integration_response(&event.idempotency_key)? {
        println!("{}", existing);
        return Ok(());
    }
    db.record_integration_response(&event.idempotency_key, run_id, &event.event, &response_json)?;
    println!("{}", response_json);
    Ok(())
}

fn process_event(event: &AgentEvent) -> Result<(Option<i64>, Value)> {
    match event.event.as_str() {
        "run_started" => {
            let external_id = event
                .external_run_id
                .as_deref()
                .unwrap_or(&event.idempotency_key);
            let run = runs::cmd_run_start(
                StartRunOptions {
                    name: event.name.as_deref(),
                    actor: event.actor.as_deref().or(Some("agent")),
                    agent: event.agent.as_deref(),
                    command: event.command.as_deref(),
                    intent: event.intent.as_deref(),
                    external_id: Some(external_id),
                },
                Output::Silent,
            )?;
            Ok((
                Some(run.id),
                json!({
                    "version": AGENT_EVENT_VERSION,
                    "event": "run_started",
                    "run_id": run.public_id(),
                    "status": run.status,
                }),
            ))
        }
        "checkpoint" => {
            let (db, project, _) = runs::prepare_project_boundary()?;
            let run = required_active_run(&db, project.id, event.run_id.as_deref())?;
            let name = required_checkpoint_name(event)?;
            let (checkpoint, created) =
                db.create_checkpoint_now(project.id, Some(run.id), name, event.intent.as_deref())?;
            Ok((
                Some(run.id),
                json!({
                    "version": AGENT_EVENT_VERSION,
                    "event": "checkpoint",
                    "run_id": run.public_id(),
                    "checkpoint_id": checkpoint.public_id(),
                    "created": created,
                }),
            ))
        }
        "intent_started" => {
            let (db, project, _) = runs::prepare_project_boundary()?;
            let run = required_active_run(&db, project.id, event.run_id.as_deref())?;
            let label = required_intent(event)?;
            let intent = db.start_run_intent(run.id, label)?;
            Ok((
                Some(run.id),
                json!({
                    "version": AGENT_EVENT_VERSION,
                    "event": "intent_started",
                    "run_id": run.public_id(),
                    "intent_id": intent.public_id(),
                    "intent": intent.label,
                }),
            ))
        }
        "intent_completed" => {
            let (db, project, _) = runs::prepare_project_boundary()?;
            let run = required_active_run(&db, project.id, event.run_id.as_deref())?;
            let intent = db.complete_run_intent(run.id, event.intent.as_deref())?;
            Ok((
                Some(run.id),
                json!({
                    "version": AGENT_EVENT_VERSION,
                    "event": "intent_completed",
                    "run_id": run.public_id(),
                    "intent_id": intent.public_id(),
                    "intent": intent.label,
                }),
            ))
        }
        "run_completed" => {
            let run = runs::cmd_run_stop(
                event.run_id.as_deref(),
                event.status.as_deref().unwrap_or("completed"),
                Output::Silent,
            )?;
            Ok((
                Some(run.id),
                json!({
                    "version": AGENT_EVENT_VERSION,
                    "event": "run_completed",
                    "run_id": run.public_id(),
                    "status": run.status,
                }),
            ))
        }
        _ => unreachable!("validated event type"),
    }
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
    if event.version != AGENT_EVENT_VERSION {
        anyhow::bail!(
            "unsupported event version {}; supported version is {}",
            event.version,
            AGENT_EVENT_VERSION
        );
    }
    if !matches!(
        event.event.as_str(),
        "run_started" | "checkpoint" | "intent_started" | "intent_completed" | "run_completed"
    ) {
        anyhow::bail!(
            "unsupported event '{}'; expected run_started, checkpoint, \
             intent_started, intent_completed, or run_completed",
            event.event
        );
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
        ("run_id", event.run_id.as_deref(), 200),
    ] {
        if value.is_some_and(|value| value.len() > max) {
            anyhow::bail!("{} exceeds {} UTF-8 bytes", name, max);
        }
    }
    if event.event != "run_started" && event.external_run_id.is_some() {
        anyhow::bail!("external_run_id is only valid for run_started");
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
        }
    }

    #[test]
    fn rejects_unknown_versions_and_events() {
        let mut wrong_version = event("run_started");
        wrong_version.version = 2;
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
}
