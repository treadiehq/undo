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
    about = "undo: give your files an undo button",
    long_about = "See what changed, compare versions, and restore files when something goes wrong."
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
    /// Start saving history for the current folder
    Start {
        /// Skip safety checks (ownership, file-count limit)
        #[arg(long)]
        force: bool,
    },

    /// Show recent file activity
    Timeline {
        /// Maximum number of events to show (minimum 1)
        #[arg(long, default_value = "20", value_parser = parse_positive_usize)]
        limit: usize,
        /// Show events since a duration ago (e.g. 2h, 1d)
        #[arg(long)]
        since: Option<String>,
        /// Highlight rapid change bursts
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

    /// Preview a restore without writing files
    Preview {
        /// File or directory path to preview
        path: String,
        /// How far back to preview from (e.g. 10m, 1h)
        duration: String,
    },

    /// Bring back an older version of a file
    Restore {
        /// File or directory path to restore
        path: Option<String>,
        /// How far back to restore from (e.g. 10m, 1h)
        duration: Option<String>,
        /// Preview the restore without writing files
        #[arg(long)]
        preview: bool,
        /// Restore from a named checkpoint
        #[arg(short = 'c', long, visible_alias = "cp")]
        checkpoint: Option<String>,
        /// Recover the latest deleted version of this path
        #[arg(long)]
        deleted: bool,
        /// Required for multi-file restores
        #[arg(long)]
        yes: bool,
    },

    /// Create a named checkpoint for the current project
    #[command(visible_alias = "mark")]
    Checkpoint {
        /// Checkpoint name
        name: String,
    },

    /// List checkpoints for the current project
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

    /// Show a guided recovery dashboard
    Panic {
        /// Apply the suggested restore before the latest burst
        #[arg(long, visible_alias = "undo-burst")]
        restore_before_latest_burst: bool,
        /// Required with --restore-before-latest-burst
        #[arg(long)]
        yes: bool,
    },

    /// Start, stop, and inspect semantic rollback sessions
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },

    /// List semantic rollback sessions
    Sessions,

    /// Preview or apply recovery for a session or change group
    Recover(RecoverArgs),

    /// Turn rollback intent into a preview-first recovery proposal
    Ask(AskArgs),

    /// Show whether Undo is running and how much space it uses
    Status,

    /// Stop saving history
    Stop {
        /// Stop Undo in every watched folder
        #[arg(long)]
        all: bool,
    },

    /// Delete old saved history
    Prune {
        /// Keep this much history for this cleanup run (e.g. 30d, 12h)
        #[arg(long)]
        keep: Option<String>,
        /// Show what would be deleted without deleting it
        #[arg(long)]
        dry_run: bool,
    },

    /// Update undo to the latest release
    Update,
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// Start a named recovery session
    Start {
        /// Session name, e.g. agent-auth-work
        name: String,
    },
    /// Stop the active recovery session
    Stop,
    /// Show one recovery session and its change groups
    Show {
        /// Session name
        name: String,
    },
}

#[derive(Args)]
pub struct RecoverArgs {
    /// Session name to recover from
    #[arg(long)]
    pub session: String,
    /// Optional group id to recover instead of the whole session
    #[arg(long)]
    pub group: Option<String>,
    /// Preview the recovery plan without writing files
    #[arg(long)]
    pub preview: bool,
    /// Required to apply a recovery plan
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct AskArgs {
    /// Natural-language rollback request
    pub query: String,
    /// Session name to search. Defaults to the latest session.
    #[arg(long)]
    pub session: Option<String>,
    /// Apply the proposal. Without this, ask only previews.
    #[arg(long)]
    pub apply: bool,
    /// Required with --apply
    #[arg(long)]
    pub yes: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
                deleted,
                yes,
            } => {
                assert_eq!(path.as_deref(), Some("."));
                assert_eq!(duration, None);
                assert!(preview);
                assert_eq!(checkpoint.as_deref(), Some("before-agent"));
                assert!(!deleted);
                assert!(yes);
            }
            _ => panic!("expected restore command"),
        }
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
                assert_eq!(args.session, "agent-auth-work");
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
                assert_eq!(args.query, "undo the auth refactor but keep security");
                assert_eq!(args.session.as_deref(), Some("agent-auth-work"));
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
                assert_eq!(args.query, "revert everything except bug fixes");
                assert!(args.session.is_none());
                assert!(args.apply);
                assert!(args.yes);
            }
            _ => panic!("expected ask command"),
        }
    }
}
