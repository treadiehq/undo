use clap::{Parser, Subcommand};

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
    about = "undo — filesystem history for your working directory",
    long_about = "See what changed. Diff it. Restore it. No git commit required."
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
    /// Start watching the current directory
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
    },

    /// Show what changed in a time window (e.g. 5m, 2h, 1d)
    WhatChanged {
        /// Duration like 5m, 30m, 2h, 1d
        duration: String,
    },

    /// Show diff of a file against its latest snapshot
    Diff {
        /// File path to diff
        path: String,
    },

    /// Restore a file from a snapshot
    Restore {
        /// File path to restore
        path: String,
        /// How far back to restore from (e.g. 10m, 1h)
        duration: String,
    },

    /// Show daemon and project status
    Status,

    /// Stop the daemon
    Stop {
        /// Stop all running undo daemons
        #[arg(long)]
        all: bool,
    },

    /// Remove old history beyond the retention window
    Prune {
        /// Override retention period (e.g. 30d, 12h)
        #[arg(long)]
        keep: Option<String>,
        /// Dry run — show what would be deleted without deleting
        #[arg(long)]
        dry_run: bool,
    },

    /// Update undo to the latest release
    Update,
}
