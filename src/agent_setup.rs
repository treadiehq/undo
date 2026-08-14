use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::Agent;

const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
];
const CURSOR_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionEnd",
    "preToolUse",
    "postToolUse",
    "postToolUseFailure",
];
const CODEX_EVENTS: &[&str] = &["SessionStart", "SessionEnd", "PreToolUse", "PostToolUse"];
const HOOK_MARKER: &str = "UNDO_AGENT_HOOK=1";

#[derive(Clone, Copy)]
enum HookSchema {
    Nested,
    Direct,
}

pub fn cmd_setup(agent: Agent) -> Result<()> {
    let executable = std::env::current_exe().context("cannot locate the Undo executable")?;
    let config_path = config_path(agent)?;
    install_at(agent, &config_path, &executable)?;

    let cwd = std::env::current_dir()?.canonicalize()?;
    let project_root = project_root_from(&cwd)?;
    let recording_root = crate::daemon::ensure_recording_for_path(&project_root)?;

    println!(
        "Installed global {} hooks in {}.",
        agent.as_str(),
        config_path.display()
    );
    println!(
        "Undo is recording file changes in {}.",
        recording_root.display()
    );
    Ok(())
}

fn config_path(agent: Agent) -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(match agent {
        Agent::Claude => home.join(".claude/settings.json"),
        Agent::Cursor => home.join(".cursor/hooks.json"),
        Agent::Codex => home.join(".codex/hooks.json"),
    })
}

fn install_at(agent: Agent, path: &Path, executable: &Path) -> Result<()> {
    let mut config = if path.try_exists()? {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str::<Value>(&contents)
            .with_context(|| format!("{} contains malformed JSON", path.display()))?
    } else {
        json!({})
    };

    merge_hooks(&mut config, agent, executable)
        .with_context(|| format!("{} has an incompatible hook structure", path.display()))?;
    write_atomic_json(path, &config)
}

fn merge_hooks(config: &mut Value, agent: Agent, executable: &Path) -> Result<()> {
    let root = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the top-level JSON value must be an object"))?;

    if agent == Agent::Cursor {
        match root.get("version") {
            Some(Value::Number(version)) if version.as_u64() == Some(1) => {}
            Some(_) => anyhow::bail!("Cursor hook version must be 1"),
            None => {
                root.insert("version".to_string(), json!(1));
            }
        }
    }

    let hooks = object_field(root, "hooks")?;
    let command = hook_command(executable, agent)?;
    let (events, schema) = match agent {
        Agent::Claude => (CLAUDE_EVENTS, HookSchema::Nested),
        Agent::Cursor => (CURSOR_EVENTS, HookSchema::Direct),
        Agent::Codex => (CODEX_EVENTS, HookSchema::Nested),
    };

    for event in events {
        let entries = array_field(hooks, event)?;
        match schema {
            HookSchema::Nested => merge_nested_event(entries, agent, event, &command)?,
            HookSchema::Direct => merge_direct_event(entries, agent, event, &command)?,
        }
    }
    Ok(())
}

fn object_field<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    if !object.contains_key(key) {
        object.insert(key.to_string(), json!({}));
    }
    object
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("'{}' must be an object", key))
}

fn array_field<'a>(object: &'a mut Map<String, Value>, key: &str) -> Result<&'a mut Vec<Value>> {
    if !object.contains_key(key) {
        object.insert(key.to_string(), json!([]));
    }
    object
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("hook event '{}' must be an array", key))
}

fn merge_nested_event(
    entries: &mut Vec<Value>,
    agent: Agent,
    event: &str,
    command: &str,
) -> Result<()> {
    let mut preserved = Vec::with_capacity(entries.len() + 1);
    for mut entry in std::mem::take(entries) {
        let group = entry
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("nested hook entries must be objects"))?;
        let commands = group
            .get_mut("hooks")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow::anyhow!("nested hook entries must contain a 'hooks' array"))?;

        for hook in commands.iter() {
            validate_command_hook(hook)?;
        }
        let original_len = commands.len();
        commands.retain(|hook| !is_undo_hook(hook, agent));
        let removed_undo = commands.len() != original_len;
        if !(removed_undo && commands.is_empty()) {
            preserved.push(entry);
        }
    }

    let mut managed = json!({
        "hooks": [{
            "type": "command",
            "command": command,
        }]
    });
    if let Some(matcher) = tool_matcher(agent, event) {
        managed["matcher"] = Value::String(matcher.to_string());
    }
    preserved.push(managed);
    *entries = preserved;
    Ok(())
}

fn merge_direct_event(
    entries: &mut Vec<Value>,
    agent: Agent,
    event: &str,
    command: &str,
) -> Result<()> {
    for hook in entries.iter() {
        validate_command_hook(hook)?;
    }
    entries.retain(|hook| !is_undo_hook(hook, agent));
    let mut managed = json!({ "command": command });
    if let Some(matcher) = tool_matcher(agent, event) {
        managed["matcher"] = Value::String(matcher.to_string());
    }
    entries.push(managed);
    Ok(())
}

fn tool_matcher(agent: Agent, event: &str) -> Option<&'static str> {
    let normalized = event.to_ascii_lowercase();
    if !normalized.contains("tooluse") {
        return None;
    }
    Some(match agent {
        Agent::Codex => "apply_patch|Edit|Write",
        Agent::Claude | Agent::Cursor => "Edit|Write|NotebookEdit|ApplyPatch",
    })
}

fn validate_command_hook(hook: &Value) -> Result<()> {
    let hook = hook
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("command hook entries must be objects"))?;
    if hook
        .get("command")
        .is_some_and(|command| !command.is_string())
    {
        anyhow::bail!("a hook 'command' must be a string");
    }
    Ok(())
}

fn is_undo_hook(hook: &Value, agent: Agent) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| {
            command.contains(HOOK_MARKER)
                || command.contains(&format!("_hook --agent {}", agent.as_str()))
        })
}

fn hook_command(executable: &Path, agent: Agent) -> Result<String> {
    if !executable.is_absolute() {
        anyhow::bail!("Undo executable path must be absolute");
    }
    let executable = executable
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Undo executable path is not valid UTF-8"))?;
    Ok(format!(
        "{} {} _hook --agent {} >/dev/null 2>&1 || true",
        HOOK_MARKER,
        shell_quote(executable),
        agent.as_str()
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_atomic_json(path: &Path, value: &Value) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;

    let mut contents = serde_json::to_vec_pretty(value)?;
    contents.push(b'\n');

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    temporary.write_all(&contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

pub fn project_root_from(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("cannot resolve project folder {}", start.display()))?;
    let directory = if start.is_dir() {
        start
    } else {
        start
            .parent()
            .ok_or_else(|| anyhow::anyhow!("project path has no parent"))?
            .to_path_buf()
    };

    for ancestor in directory.ancestors() {
        if ancestor.join(".git").try_exists()? {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn command_count(value: &Value) -> usize {
        match value {
            Value::Array(values) => values.iter().map(command_count).sum(),
            Value::Object(values) => {
                let here = values
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(HOOK_MARKER))
                    as usize;
                here + values.values().map(command_count).sum::<usize>()
            }
            _ => 0,
        }
    }

    #[test]
    fn creates_each_agents_expected_schema() {
        let directory = tempfile::tempdir().unwrap();
        let executable = Path::new("/opt/Undo Tools/undo");

        for agent in [Agent::Claude, Agent::Cursor, Agent::Codex] {
            let path = directory.path().join(agent.as_str()).join("config.json");
            install_at(agent, &path, executable).unwrap();
            let config: Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

            let expected_events = match agent {
                Agent::Claude => CLAUDE_EVENTS,
                Agent::Cursor => CURSOR_EVENTS,
                Agent::Codex => CODEX_EVENTS,
            };
            assert_eq!(command_count(&config), expected_events.len());
            assert!(config["hooks"].is_object());
            if agent == Agent::Cursor {
                assert_eq!(config["version"], 1);
                assert!(config["hooks"]["sessionStart"][0]["command"].is_string());
                assert_eq!(
                    config["hooks"]["preToolUse"][0]["matcher"],
                    "Edit|Write|NotebookEdit|ApplyPatch"
                );
            } else {
                assert!(config["hooks"]["SessionStart"][0]["hooks"][0]["command"].is_string());
                assert!(config["hooks"]["PreToolUse"][0]["matcher"].is_string());
            }
            if agent == Agent::Codex {
                assert!(config["hooks"].get("PostToolUseFailure").is_none());
                assert!(config["hooks"]["SessionStart"][0].get("matcher").is_none());
                assert_eq!(
                    config["hooks"]["PreToolUse"][0]["matcher"],
                    "apply_patch|Edit|Write"
                );
            }
        }
    }

    #[test]
    fn preserves_unrelated_config_and_hooks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
  "theme": "dark",
  "hooks": {
    "SessionStart": [{
      "matcher": "startup",
      "hooks": [{"type": "command", "command": "printf existing"}]
    }],
    "Notification": [{"hooks": [{"type": "command", "command": "notify"}]}]
  }
}"#,
        )
        .unwrap();

        install_at(Agent::Claude, &path, Path::new("/usr/local/bin/undo")).unwrap();
        let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(
            config["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "printf existing"
        );
        assert_eq!(
            config["hooks"]["Notification"][0]["hooks"][0]["command"],
            "notify"
        );
    }

    #[test]
    fn repeated_install_is_idempotent_and_updates_undo_commands() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hooks.json");
        std::fs::write(
            &path,
            r#"{"version":1,"hooks":{"sessionStart":[{"command":"UNDO_AGENT_HOOK=1 '/old/undo' _hook --agent cursor >/dev/null 2>&1 || true"}]}}"#,
        )
        .unwrap();

        let executable = Path::new("/new/undo");
        install_at(Agent::Cursor, &path, executable).unwrap();
        let first = std::fs::read(&path).unwrap();
        install_at(Agent::Cursor, &path, executable).unwrap();
        let second = std::fs::read(&path).unwrap();
        assert_eq!(first, second);

        let config: Value = serde_json::from_slice(&second).unwrap();
        assert_eq!(command_count(&config), CURSOR_EVENTS.len());
        assert!(
            config["hooks"]["sessionStart"][0]["command"]
                .as_str()
                .unwrap()
                .contains("'/new/undo'")
        );
    }

    #[test]
    fn malformed_or_incompatible_input_is_left_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let malformed = directory.path().join("malformed.json");
        std::fs::write(&malformed, b"{ not json").unwrap();
        let before = std::fs::read(&malformed).unwrap();
        assert!(install_at(Agent::Claude, &malformed, Path::new("/undo")).is_err());
        assert_eq!(std::fs::read(&malformed).unwrap(), before);

        let incompatible = directory.path().join("incompatible.json");
        std::fs::write(&incompatible, br#"{"hooks":{"SessionStart":"wrong"}}"#).unwrap();
        let before = std::fs::read(&incompatible).unwrap();
        assert!(install_at(Agent::Claude, &incompatible, Path::new("/undo")).is_err());
        assert_eq!(std::fs::read(&incompatible).unwrap(), before);
    }

    #[test]
    fn executable_is_safely_shell_quoted() {
        let command =
            hook_command(Path::new("/Applications/Undo's Tools/undo"), Agent::Claude).unwrap();
        assert_eq!(
            command,
            "UNDO_AGENT_HOOK=1 '/Applications/Undo'\"'\"'s Tools/undo' _hook --agent claude >/dev/null 2>&1 || true"
        );
        assert!(hook_command(Path::new("relative/undo"), Agent::Claude).is_err());
    }

    #[test]
    fn new_parent_and_config_permissions_are_private() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("nested").join(".claude");
        let path = parent.join("settings.json");
        install_at(Agent::Claude, &path, Path::new("/undo")).unwrap();

        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn project_root_walks_to_git_marker_and_falls_back_to_start() {
        let repository = tempfile::tempdir().unwrap();
        std::fs::create_dir(repository.path().join(".git")).unwrap();
        let nested = repository.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            project_root_from(&nested).unwrap(),
            repository.path().canonicalize().unwrap()
        );

        let plain = tempfile::tempdir().unwrap();
        assert_eq!(
            project_root_from(plain.path()).unwrap(),
            plain.path().canonicalize().unwrap()
        );
    }
}
