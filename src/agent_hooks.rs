use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::cli::Agent;
use crate::db::Database;

#[derive(Debug, Deserialize)]
struct HookPayload {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    workspace_roots: Vec<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default, alias = "tool")]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
    #[serde(default)]
    reason: Option<String>,
}

pub fn cmd_hook(agent: Agent) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let payload = parse_payload(&input)?;
    handle_payload(agent, &payload, &std::env::current_dir()?)
}

fn parse_payload(input: &str) -> Result<HookPayload> {
    if input.trim().is_empty() {
        anyhow::bail!("agent hook requires one JSON payload on stdin");
    }
    serde_json::from_str(input).context("invalid agent hook JSON")
}

fn handle_payload(agent: Agent, payload: &HookPayload, process_cwd: &Path) -> Result<()> {
    let Some(event) = canonical_event(payload.hook_event_name.as_deref()) else {
        return Ok(());
    };
    let Some(external_run_id) = stable_external_run_id(agent, payload) else {
        // Some host versions omit both stable identifiers. Starting an
        // unresolvable Run would be worse than leaving history unattributed.
        return Ok(());
    };

    let payload_root = select_payload_root(payload, process_cwd)?;
    let git_root = crate::agent_setup::project_root_from(&payload_root)?;
    let recording_root = crate::daemon::ensure_recording_for_path(&git_root)?;
    let db = Database::open()?;
    let project = crate::find_project(&db, &recording_root)?;

    match event.as_str() {
        "sessionstart" => {
            crate::runs::sync_project(&db, &project, &recording_root)?;
            ensure_hook_run(&db, project.id, &external_run_id, agent)?;
        }
        "sessionend" => {
            if is_transitional_end(payload.reason.as_deref()) {
                return Ok(());
            }
            if let Some(run) = db.get_run_by_external_id(project.id, &external_run_id)?
                && run.is_active()
            {
                // Session-end hooks have a very small host timeout. Mutating
                // tool post-hooks already reconcile exact paths, so completion
                // must stay bounded instead of walking the project here.
                db.complete_run(run.id, "completed")?;
            }
        }
        "pretooluse" => {
            let Some(change_id) = payload.tool_use_id.as_deref() else {
                return Ok(());
            };
            let paths = extract_normalized_paths(payload, &recording_root, &project.root_path)?;
            if paths.is_empty() {
                return Ok(());
            }
            crate::watcher::reconcile_paths(&db, &project, &recording_root, &paths)?;
            // Cursor starts session hooks asynchronously. Creating the Run here
            // as well closes the race where the first tool starts before its
            // fire-and-forget SessionStart hook has finished.
            let run = ensure_hook_run(&db, project.id, &external_run_id, agent)?;
            db.open_run_boundary(run.id, change_id, &paths)?;
        }
        "posttooluse" | "posttoolusefailure" => {
            let Some(change_id) = payload.tool_use_id.as_deref() else {
                return Ok(());
            };
            let paths = extract_normalized_paths(payload, &recording_root, &project.root_path)?;
            if paths.is_empty() {
                return Ok(());
            }
            crate::watcher::reconcile_paths(&db, &project, &recording_root, &paths)?;
            let run = required_hook_run(&db, project.id, &external_run_id)?;
            if db
                .get_run_boundary_by_external_id(run.id, change_id)?
                .is_some()
            {
                db.close_run_boundary(run.id, change_id, &paths)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn canonical_event(event: Option<&str>) -> Option<String> {
    let event = event?
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(
        event.as_str(),
        "sessionstart" | "sessionend" | "pretooluse" | "posttooluse" | "posttoolusefailure"
    )
    .then_some(event)
}

fn select_payload_root(payload: &HookPayload, process_cwd: &Path) -> Result<PathBuf> {
    for workspace_root in &payload.workspace_roots {
        if let Some(path) = usable_directory(workspace_root, process_cwd) {
            return Ok(path);
        }
    }
    if let Some(cwd) = payload.cwd.as_deref()
        && let Some(path) = usable_directory(cwd, process_cwd)
    {
        return Ok(path);
    }
    process_cwd
        .canonicalize()
        .context("cannot resolve the hook process working directory")
}

fn usable_directory(value: &str, process_cwd: &Path) -> Option<PathBuf> {
    let value = value.trim().strip_prefix("file://").unwrap_or(value.trim());
    if value.is_empty() {
        return None;
    }
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        process_cwd.join(path)
    };
    path.canonicalize().ok().filter(|path| path.is_dir())
}

fn stable_external_run_id(agent: Agent, payload: &HookPayload) -> Option<String> {
    payload
        .conversation_id
        .as_deref()
        .or(payload.session_id.as_deref())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| format!("{}:{}", agent.as_str(), id))
}

fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude Code",
        Agent::Cursor => "Cursor",
        Agent::Codex => "Codex",
    }
}

fn is_transitional_end(reason: Option<&str>) -> bool {
    reason.map(str::to_ascii_lowercase).is_some_and(|reason| {
        ["resume", "compact", "continue"]
            .iter()
            .any(|marker| reason.contains(marker))
    })
}

fn required_hook_run(
    db: &Database,
    project_id: i64,
    external_run_id: &str,
) -> Result<crate::models::Session> {
    let run = db
        .get_run_by_external_id(project_id, external_run_id)?
        .ok_or_else(|| anyhow::anyhow!("reported hook Run '{}' not found", external_run_id))?;
    if !run.is_active() {
        anyhow::bail!("reported hook Run {} is already complete", run.public_id());
    }
    Ok(run)
}

fn ensure_hook_run(
    db: &Database,
    project_id: i64,
    external_run_id: &str,
    agent: Agent,
) -> Result<crate::models::Session> {
    if let Some(run) = db.get_run_by_external_id(project_id, external_run_id)? {
        if run.is_active() {
            return Ok(run);
        }
        anyhow::bail!("reported hook Run {} is already complete", run.public_id());
    }
    db.start_reported_run(
        project_id,
        &format!("{} session {}", agent_label(agent), external_run_id),
        "hook",
        "agent",
        Some(agent_label(agent)),
        None,
        None,
        external_run_id,
    )
}

fn extract_normalized_paths(
    payload: &HookPayload,
    root: &Path,
    project_root: &str,
) -> Result<Vec<String>> {
    let Some(tool_name) = payload.tool_name.as_deref() else {
        return Ok(Vec::new());
    };
    if !is_mutating_tool(tool_name) {
        return Ok(Vec::new());
    }
    let Some(input) = payload.tool_input.as_ref() else {
        return Ok(Vec::new());
    };
    let mut candidates = extract_exact_paths(input);
    let normalized_tool = tool_name.to_ascii_lowercase().replace(['_', '-'], "");
    if normalized_tool.contains("applypatch")
        && let Some(command) = input.get("command").and_then(Value::as_str)
    {
        let mut patch_paths = BTreeSet::new();
        extract_apply_patch_paths(command, &mut patch_paths);
        candidates.extend(patch_paths);
    }
    let mut normalized = BTreeSet::new();
    for candidate in candidates {
        let candidate = candidate.trim();
        if candidate.is_empty()
            || candidate
                .chars()
                .any(|character| "*?[]{}".contains(character))
        {
            continue;
        }
        let resolved = crate::safe_resolve_path(root, candidate, project_root)?;
        if resolved.is_dir() {
            continue;
        }
        normalized.insert(resolved.to_string_lossy().into_owned());
    }
    Ok(normalized.into_iter().collect())
}

fn is_mutating_tool(tool_name: &str) -> bool {
    let normalized = tool_name.to_ascii_lowercase().replace(['_', '-'], "");
    [
        "write",
        "edit",
        "applypatch",
        "delete",
        "rename",
        "movefile",
        "createfile",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn extract_exact_paths(input: &Value) -> Vec<String> {
    const PATH_KEYS: &[&str] = &[
        "file_path",
        "path",
        "target_path",
        "target_file",
        "target_notebook",
        "source_path",
        "destination_path",
        "old_path",
        "new_path",
    ];
    const PATCH_KEYS: &[&str] = &["patch", "input", "patch_text"];

    let mut paths = BTreeSet::new();
    if let Some(patch) = input.as_str() {
        extract_apply_patch_paths(patch, &mut paths);
    }
    if let Some(object) = input.as_object() {
        for key in PATH_KEYS {
            if let Some(value) = object.get(*key) {
                match value {
                    Value::String(path) => {
                        paths.insert(path.clone());
                    }
                    Value::Array(values) => {
                        for path in values.iter().filter_map(Value::as_str) {
                            paths.insert(path.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        for key in PATCH_KEYS {
            if let Some(patch) = object.get(*key).and_then(Value::as_str) {
                extract_apply_patch_paths(patch, &mut paths);
            }
        }
    }
    paths.into_iter().collect()
}

fn extract_apply_patch_paths(patch: &str, paths: &mut BTreeSet<String>) {
    const HEADERS: &[&str] = &[
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    for line in patch.lines().map(str::trim) {
        for header in HEADERS {
            if let Some(path) = line.strip_prefix(header) {
                if let Some((source, target)) = path.split_once(" -> ") {
                    for endpoint in [source.trim(), target.trim()] {
                        if !endpoint.is_empty() {
                            paths.insert(endpoint.to_string());
                        }
                    }
                } else {
                    let path = path.trim();
                    if !path.is_empty() {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_hook_fields() {
        let payload = parse_payload(
            r#"{
                "cwd": "/project",
                "workspace_roots": ["/workspace/a", "/workspace/b"],
                "session_id": "session-1",
                "conversation_id": "conversation-2",
                "hook_event_name": "PreToolUse",
                "tool_name": "Write",
                "tool_use_id": "tool-3",
                "tool_input": {"path": "src/main.rs"},
                "reason": "permission check",
                "agent_specific_extra": true
            }"#,
        )
        .unwrap();

        assert_eq!(payload.cwd.as_deref(), Some("/project"));
        assert_eq!(payload.workspace_roots.len(), 2);
        assert_eq!(payload.session_id.as_deref(), Some("session-1"));
        assert_eq!(payload.conversation_id.as_deref(), Some("conversation-2"));
        assert_eq!(payload.hook_event_name.as_deref(), Some("PreToolUse"));
        assert_eq!(payload.tool_name.as_deref(), Some("Write"));
        assert_eq!(payload.tool_use_id.as_deref(), Some("tool-3"));
        assert_eq!(payload.tool_input.unwrap()["path"], "src/main.rs");
        assert_eq!(payload.reason.as_deref(), Some("permission check"));
    }

    #[test]
    fn rejects_empty_malformed_and_multiple_payloads() {
        assert!(parse_payload("").is_err());
        assert!(parse_payload("{").is_err());
        assert!(parse_payload(r#"{"cwd":"/one"} {"cwd":"/two"}"#).is_err());
    }

    #[test]
    fn recognizes_agent_session_start_spellings_only() {
        for spelling in ["SessionStart", "sessionStart", "session_start"] {
            assert_eq!(
                canonical_event(Some(spelling)).as_deref(),
                Some("sessionstart")
            );
        }
        assert_eq!(
            canonical_event(Some("SessionEnd")).as_deref(),
            Some("sessionend")
        );
        assert_eq!(
            canonical_event(Some("PreToolUse")).as_deref(),
            Some("pretooluse")
        );
        assert!(canonical_event(None).is_none());
    }

    #[test]
    fn workspace_root_precedes_payload_and_process_cwd() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let payload_cwd = directory.path().join("payload-cwd");
        let process_cwd = directory.path().join("process-cwd");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&payload_cwd).unwrap();
        std::fs::create_dir_all(&process_cwd).unwrap();

        let payload = HookPayload {
            cwd: Some(payload_cwd.to_string_lossy().into_owned()),
            workspace_roots: vec![workspace.to_string_lossy().into_owned()],
            session_id: None,
            conversation_id: None,
            hook_event_name: Some("SessionStart".to_string()),
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            reason: None,
        };
        assert_eq!(
            select_payload_root(&payload, &process_cwd).unwrap(),
            workspace.canonicalize().unwrap()
        );
    }

    #[test]
    fn invalid_workspace_root_falls_back_to_payload_cwd_then_process_cwd() {
        let directory = tempfile::tempdir().unwrap();
        let payload_cwd = directory.path().join("payload-cwd");
        let process_cwd = directory.path().join("process-cwd");
        std::fs::create_dir_all(&payload_cwd).unwrap();
        std::fs::create_dir_all(&process_cwd).unwrap();

        let mut payload = HookPayload {
            cwd: Some(payload_cwd.to_string_lossy().into_owned()),
            workspace_roots: vec!["/definitely/missing/undo-workspace".to_string()],
            session_id: None,
            conversation_id: None,
            hook_event_name: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            reason: None,
        };
        assert_eq!(
            select_payload_root(&payload, &process_cwd).unwrap(),
            payload_cwd.canonicalize().unwrap()
        );

        payload.cwd = Some("/also/missing".to_string());
        assert_eq!(
            select_payload_root(&payload, &process_cwd).unwrap(),
            process_cwd.canonicalize().unwrap()
        );
    }

    #[test]
    fn workspace_file_uri_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let payload = HookPayload {
            cwd: None,
            workspace_roots: vec![format!("file://{}", directory.path().display())],
            session_id: None,
            conversation_id: None,
            hook_event_name: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            reason: None,
        };
        assert_eq!(
            select_payload_root(&payload, Path::new("/")).unwrap(),
            directory.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn stable_run_identity_prefers_conversation_and_namespaces_agent() {
        let payload = HookPayload {
            cwd: None,
            workspace_roots: Vec::new(),
            session_id: Some("session-1".to_string()),
            conversation_id: Some("conversation-2".to_string()),
            hook_event_name: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            reason: None,
        };
        assert_eq!(
            stable_external_run_id(Agent::Cursor, &payload).as_deref(),
            Some("cursor:conversation-2")
        );
    }

    #[test]
    fn extracts_only_explicit_mutating_paths_and_patch_headers() {
        let input = serde_json::json!({
            "file_path": "src/main.rs",
            "command": "touch hidden.rs",
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** Add File: docs/new.md\n*** End Patch"
        });
        assert_eq!(
            extract_exact_paths(&input),
            vec![
                "docs/new.md".to_string(),
                "src/lib.rs".to_string(),
                "src/main.rs".to_string()
            ]
        );
    }

    #[test]
    fn read_tools_never_open_change_boundaries() {
        assert!(!is_mutating_tool("Read"));
        assert!(!is_mutating_tool("Grep"));
        assert!(!is_mutating_tool("Shell"));
        assert!(is_mutating_tool("Write"));
        assert!(is_mutating_tool("NotebookEdit"));
        assert!(is_mutating_tool("apply_patch"));
    }

    #[test]
    fn codex_apply_patch_command_yields_exact_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("src/main.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old").unwrap();
        let payload = HookPayload {
            cwd: None,
            workspace_roots: Vec::new(),
            session_id: Some("session".to_string()),
            conversation_id: None,
            hook_event_name: Some("PreToolUse".to_string()),
            tool_name: Some("apply_patch".to_string()),
            tool_use_id: Some("tool".to_string()),
            tool_input: Some(serde_json::json!({
                "command": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch"
            })),
            reason: None,
        };
        assert_eq!(
            extract_normalized_paths(&payload, root.path(), &root.path().to_string_lossy())
                .unwrap(),
            vec![path.canonicalize().unwrap().to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn arbitrary_shell_tools_never_claim_paths() {
        let root = tempfile::tempdir().unwrap();
        let payload = HookPayload {
            cwd: None,
            workspace_roots: Vec::new(),
            session_id: Some("session".to_string()),
            conversation_id: None,
            hook_event_name: Some("PreToolUse".to_string()),
            tool_name: Some("Shell".to_string()),
            tool_use_id: Some("tool".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "src/main.rs",
                "command": "printf x > src/main.rs"
            })),
            reason: None,
        };
        assert!(
            extract_normalized_paths(&payload, root.path(), &root.path().to_string_lossy())
                .unwrap()
                .is_empty()
        );
    }
}
