use clap::{Args, Parser, Subcommand};

fn parse_positive_usize(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|e| format!("{e}"))?;
    if n == 0 {
        return Err("limit must be at least 1".to_string());
    }
    Ok(n)
}

#[derive(Parser)]
#[command(
    name = "undo",
    version,
    about = "Preview and undo file changes from coding agents",
    long_about = "Record file changes, review what the agent did, and safely restore unwanted work."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Enable verbose output
    #[arg(long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start recording file changes in the current folder
    Start {
        /// Bypass ownership, file-count, and overlap safety checks
        #[arg(long)]
        force: bool,
    },

    /// Show recent file changes
    Timeline {
        /// Maximum number of file changes to show (minimum 1)
        #[arg(long, default_value = "20", value_parser = parse_positive_usize)]
        limit: usize,
        /// Show changes since a duration ago (e.g. 2h, 1d)
        #[arg(long)]
        since: Option<String>,
        /// Highlight rapid groups of file changes
        #[arg(long)]
        bursts: bool,
        /// Show only deleted-file activity
        #[arg(long)]
        deleted: bool,
    },

    /// Show what changed recently (e.g. 5m, 2h, 1d)
    WhatChanged {
        /// Duration like 5m, 30m, 2h, 1d
        duration: String,
    },

    /// Compare a file with a saved version
    Diff {
        /// File path to compare
        path: String,
        /// Optional duration like 10m, 1h
        duration: Option<String>,
        /// Compare against a named checkpoint
        #[arg(short = 'c', long, visible_alias = "cp")]
        checkpoint: Option<String>,
        /// Show only a line-count summary
        #[arg(long)]
        summary: bool,
        /// Show a compact diff stat before the diff
        #[arg(long)]
        stat: bool,
    },

    /// Preview how restoring an earlier version would change files
    Preview {
        /// File or folder to preview
        path: String,
        /// How far back to preview from (e.g. 10m, 1h)
        duration: String,
    },

    /// Restore a file or folder to an earlier version
    Restore {
        /// File or folder to restore
        path: Option<String>,
        /// How far back to restore from (e.g. 10m, 1h)
        duration: Option<String>,
        /// Preview the restore without writing files
        #[arg(long)]
        preview: bool,
        /// Restore from a named checkpoint
        #[arg(short = 'c', long, visible_alias = "cp")]
        checkpoint: Option<String>,
        /// Restore at an exact Unix timestamp in seconds
        #[arg(
            long,
            value_name = "UNIX_SECONDS",
            conflicts_with_all = ["duration", "checkpoint", "deleted"]
        )]
        timestamp: Option<i64>,
        /// Recover the latest deleted version of this path
        #[arg(long)]
        deleted: bool,
        /// Allow Undo to change files in a multi-file restore
        #[arg(long)]
        yes: bool,
    },

    /// Save a named point in history
    #[command(visible_alias = "mark")]
    Checkpoint {
        /// Checkpoint name
        name: String,
        /// Optional note describing this checkpoint
        #[arg(long)]
        intent: Option<String>,
        /// Run id or name to associate; defaults to the active Run
        #[arg(long)]
        run: Option<String>,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// List saved checkpoints for the current folder
    #[command(visible_alias = "marks")]
    Checkpoints,

    /// List recently deleted files that can be recovered
    Deleted {
        /// Maximum number of deleted files to show (minimum 1)
        #[arg(long, default_value = "20", value_parser = parse_positive_usize)]
        limit: usize,
    },

    /// Recover a deleted file from its last captured contents
    RestoreDeleted {
        /// File path to recover
        path: String,
    },

    /// Show a read-only emergency recovery summary
    Panic {
        /// Apply the suggested restore before the latest burst
        #[arg(long, visible_alias = "undo-burst")]
        restore_before_latest_burst: bool,
        /// Allow Undo to change files for the emergency restore
        #[arg(long)]
        yes: bool,
    },

    /// Record and inspect agent or command work
    #[command(
        after_help = "Examples:\n  undo run claude\n  undo run codex -- --full-auto\n  undo run start --agent \"Claude Code\" --intent \"Redesign dashboard\"\n  undo run show r_421"
    )]
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },

    /// List recorded work
    Runs {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Apply an exact saved recovery plan
    Apply {
        /// Saved recovery plan id, e.g. rec_812
        recovery: String,
    },

    /// Accept one versioned agent lifecycle event as JSON
    #[command(
        after_help = "Examples:\n  printf '%s' '{\"version\":1,\"event\":\"run_started\",\"idempotency_key\":\"task-42-start\",\"agent\":\"Claude Code\"}' | undo event\n  undo event --json '{\"version\":1,\"event\":\"run_completed\",\"idempotency_key\":\"task-42-end\",\"run_id\":\"r_421\"}'"
    )]
    Event {
        /// Inline JSON; when omitted, one JSON object is read from stdin
        #[arg(long, value_name = "OBJECT")]
        json: Option<String>,
    },

    /// Compatibility alias for the pre-Run session commands
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },

    /// Compatibility alias for `undo runs`
    Sessions,

    /// Preview or apply recovery for a Run or group of file changes
    Recover(RecoverArgs),

    /// Describe unwanted work and preview a recovery plan
    Ask(AskArgs),

    /// Show recording health and storage use
    Status,

    /// Stop recording file changes
    Stop {
        /// Stop Undo in every watched folder
        #[arg(long)]
        all: bool,
    },

    /// Delete old saved versions
    Prune {
        /// Keep this much history for this cleanup run (e.g. 30d, 12h)
        #[arg(long)]
        keep: Option<String>,
        /// Show what would be deleted and freed without changing anything
        #[arg(long)]
        dry_run: bool,
    },

    /// Update undo to the latest release
    Update,
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// Start a named Run
    Start {
        /// Session name, e.g. agent-auth-work
        name: String,
    },
    /// Finish the active Run
    Stop,
    /// Show one Run and its groups of file changes
    Show {
        /// Session name
        name: String,
    },
}

#[derive(Subcommand)]
pub enum RunCommand {
    /// Start recording a Run without launching a command
    Start {
        /// Optional human-readable name
        #[arg(long)]
        name: Option<String>,
        /// Actor type: human, agent, tool, or mixed
        #[arg(long)]
        actor: Option<String>,
        /// Agent identity, e.g. Claude Code
        #[arg(long)]
        agent: Option<String>,
        /// Optional note describing this Run
        #[arg(long)]
        intent: Option<String>,
        /// External integration id
        #[arg(long)]
        external_id: Option<String>,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Finish the active Run
    Stop {
        /// Run id or legacy name; defaults to the active Run
        reference: Option<String>,
        /// Final status: completed, failed, or aborted
        #[arg(long, default_value = "completed")]
        status: String,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Show one Run, its checkpoints, task markers, and file changes
    Show {
        /// Run id or legacy name
        reference: String,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// List recorded work
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Execute an arbitrary command inside a Run
    Exec {
        /// Override inferred agent identity
        #[arg(long)]
        agent: Option<String>,
        /// Optional human-readable Run name
        #[arg(long)]
        name: Option<String>,
        /// Optional note describing this Run
        #[arg(long)]
        intent: Option<String>,
        /// Command and arguments after `--`
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Shorthand: an unknown subcommand is executed as the Run command
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Args)]
pub struct RecoverArgs {
    /// Run id or legacy name to recover from
    #[arg(long, visible_alias = "session")]
    pub run: String,
    /// Optional file-change group to recover instead of the whole Run
    #[arg(long)]
    pub group: Option<String>,
    /// Preview the recovery plan without changing files
    #[arg(long)]
    pub preview: bool,
    /// Allow Undo to change files using the saved recovery plan
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct AskArgs {
    /// Either QUERY, or RUN_ID followed by QUERY
    #[arg(required = true, num_args = 1..=2)]
    pub input: Vec<String>,
    /// Run id or legacy name. Defaults to the latest completed Run.
    #[arg(long, visible_alias = "session")]
    pub run: Option<String>,
    /// Apply the saved plan; without this, ask only previews
    #[arg(long)]
    pub apply: bool,
    /// Allow Undo to change files with --apply
    #[arg(long)]
    pub yes: bool,
}

impl AskArgs {
    pub fn resolve(&self) -> Result<(Option<&str>, &str), String> {
        match self.input.as_slice() {
            [query] => Ok((self.run.as_deref(), query)),
            [run, query] if self.run.is_none() => Ok((Some(run), query)),
            [_, _] => Err("use either positional RUN_ID or --run, not both".to_string()),
            _ => Err("ask requires QUERY, optionally preceded by RUN_ID".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn parses_timeline_recovery_flags() {
        let cli = Cli::try_parse_from([
            "undo",
            "timeline",
            "--limit",
            "50",
            "--since",
            "30m",
            "--bursts",
            "--deleted",
        ])
        .unwrap();

        match cli.command {
            Command::Timeline {
                limit,
                since,
                bursts,
                deleted,
            } => {
                assert_eq!(limit, 50);
                assert_eq!(since.as_deref(), Some("30m"));
                assert!(bursts);
                assert!(deleted);
            }
            _ => panic!("expected timeline command"),
        }
    }

    #[test]
    fn parses_diff_checkpoint_and_summary_flags() {
        let cli = Cli::try_parse_from([
            "undo",
            "diff",
            "src/main.rs",
            "10m",
            "--checkpoint",
            "before-agent",
            "--summary",
            "--stat",
        ])
        .unwrap();

        match cli.command {
            Command::Diff {
                path,
                duration,
                checkpoint,
                summary,
                stat,
            } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(duration.as_deref(), Some("10m"));
                assert_eq!(checkpoint.as_deref(), Some("before-agent"));
                assert!(summary);
                assert!(stat);
            }
            _ => panic!("expected diff command"),
        }
    }

    #[test]
    fn parses_restore_checkpoint_directory_confirmation() {
        let cli = Cli::try_parse_from([
            "undo",
            "restore",
            ".",
            "--checkpoint",
            "before-agent",
            "--preview",
            "--yes",
        ])
        .unwrap();

        match cli.command {
            Command::Restore {
                path,
                duration,
                preview,
                checkpoint,
                timestamp,
                deleted,
                yes,
            } => {
                assert_eq!(path.as_deref(), Some("."));
                assert_eq!(duration, None);
                assert!(preview);
                assert_eq!(checkpoint.as_deref(), Some("before-agent"));
                assert_eq!(timestamp, None);
                assert!(!deleted);
                assert!(yes);
            }
            _ => panic!("expected restore command"),
        }
    }

    #[test]
    fn parses_restore_absolute_timestamp_preview() {
        let cli = Cli::try_parse_from([
            "undo",
            "restore",
            ".",
            "--timestamp",
            "1713200000",
            "--preview",
        ])
        .unwrap();

        match cli.command {
            Command::Restore {
                path,
                duration,
                preview,
                checkpoint,
                timestamp,
                deleted,
                yes,
            } => {
                assert_eq!(path.as_deref(), Some("."));
                assert_eq!(duration, None);
                assert!(preview);
                assert_eq!(checkpoint, None);
                assert_eq!(timestamp, Some(1_713_200_000));
                assert!(!deleted);
                assert!(!yes);
            }
            _ => panic!("expected restore command"),
        }
    }

    #[test]
    fn restore_timestamp_conflicts_with_relative_duration() {
        let result =
            Cli::try_parse_from(["undo", "restore", ".", "10m", "--timestamp", "1713200000"]);

        assert!(result.is_err());
    }

    #[test]
    fn parses_panic_apply_confirmation() {
        let cli = Cli::try_parse_from(["undo", "panic", "--restore-before-latest-burst", "--yes"])
            .unwrap();

        match cli.command {
            Command::Panic {
                restore_before_latest_burst,
                yes,
            } => {
                assert!(restore_before_latest_burst);
                assert!(yes);
            }
            _ => panic!("expected panic command"),
        }
    }

    #[test]
    fn parses_session_start_and_show_commands() {
        let cli = Cli::try_parse_from(["undo", "session", "start", "agent-auth-work"]).unwrap();
        match cli.command {
            Command::Session {
                command: SessionCommand::Start { name },
            } => assert_eq!(name, "agent-auth-work"),
            _ => panic!("expected session start command"),
        }

        let cli = Cli::try_parse_from(["undo", "session", "show", "agent-auth-work"]).unwrap();
        match cli.command {
            Command::Session {
                command: SessionCommand::Show { name },
            } => assert_eq!(name, "agent-auth-work"),
            _ => panic!("expected session show command"),
        }
    }

    #[test]
    fn parses_recover_session_group_preview() {
        let cli = Cli::try_parse_from([
            "undo",
            "recover",
            "--session",
            "agent-auth-work",
            "--group",
            "auth",
            "--preview",
        ])
        .unwrap();

        match cli.command {
            Command::Recover(args) => {
                assert_eq!(args.run, "agent-auth-work");
                assert_eq!(args.group.as_deref(), Some("auth"));
                assert!(args.preview);
                assert!(!args.yes);
            }
            _ => panic!("expected recover command"),
        }
    }

    #[test]
    fn parses_ask_preview_defaults() {
        let cli = Cli::try_parse_from([
            "undo",
            "ask",
            "undo the auth refactor but keep security",
            "--session",
            "agent-auth-work",
        ])
        .unwrap();

        match cli.command {
            Command::Ask(args) => {
                let (run, query) = args.resolve().unwrap();
                assert_eq!(query, "undo the auth refactor but keep security");
                assert_eq!(run, Some("agent-auth-work"));
                assert!(!args.apply);
                assert!(!args.yes);
            }
            _ => panic!("expected ask command"),
        }
    }

    #[test]
    fn parses_ask_apply_confirmation() {
        let cli = Cli::try_parse_from([
            "undo",
            "ask",
            "revert everything except bug fixes",
            "--apply",
            "--yes",
        ])
        .unwrap();

        match cli.command {
            Command::Ask(args) => {
                let (run, query) = args.resolve().unwrap();
                assert_eq!(query, "revert everything except bug fixes");
                assert!(run.is_none());
                assert!(args.apply);
                assert!(args.yes);
            }
            _ => panic!("expected ask command"),
        }
    }

    #[test]
    fn parses_canonical_run_and_ask_commands() {
        let cli = Cli::try_parse_from(["undo", "run", "claude"]).unwrap();
        match cli.command {
            Command::Run {
                command: RunCommand::External(command),
            } => assert_eq!(command, vec!["claude"]),
            _ => panic!("expected shorthand Run command"),
        }
        let cli = Cli::try_parse_from(["undo", "run", "codex", "--", "--full-auto"]).unwrap();
        match cli.command {
            Command::Run {
                command: RunCommand::External(command),
            } => assert_eq!(command, vec!["codex", "--", "--full-auto"]),
            _ => panic!("expected shorthand Run command with arguments"),
        }
        let cli = Cli::try_parse_from([
            "undo",
            "run",
            "exec",
            "--",
            "cargo",
            "test",
            "--",
            "--nocapture",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                command: RunCommand::Exec { command, .. },
            } => assert_eq!(command, vec!["cargo", "test", "--", "--nocapture"]),
            _ => panic!("expected explicit Run command"),
        }

        let cli =
            Cli::try_parse_from(["undo", "ask", "r_421", "remove the database migration work"])
                .unwrap();
        match cli.command {
            Command::Ask(args) => {
                let (run, query) = args.resolve().unwrap();
                assert_eq!(run, Some("r_421"));
                assert_eq!(query, "remove the database migration work");
            }
            _ => panic!("expected ask command"),
        }
    }

    #[test]
    fn help_explains_file_change_permissions_and_safety_bypasses() {
        let mut command = Cli::command();
        let start_help = command
            .find_subcommand_mut("start")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(start_help.contains("Bypass ownership, file-count, and overlap safety checks"));

        let restore_help = command
            .find_subcommand_mut("restore")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(restore_help.contains("Allow Undo to change files in a multi-file restore"));
    }

    #[test]
    fn metadata_intent_help_does_not_call_it_an_explicit_intent() {
        let mut command = Cli::command();
        let checkpoint_help = command
            .find_subcommand_mut("checkpoint")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(checkpoint_help.contains("Optional note describing this checkpoint"));
        assert!(!checkpoint_help.contains("explicit intent"));

        let run_help = command
            .find_subcommand_mut("run")
            .unwrap()
            .find_subcommand_mut("start")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(run_help.contains("Optional note describing this Run"));
        assert!(!run_help.contains("explicit intent"));
    }
}
