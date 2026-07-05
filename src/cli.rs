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
    },

    /// Show what changed recently (e.g. 5m, 2h, 1d)
    WhatChanged {
        /// Duration like 5m, 30m, 2h, 1d
        duration: String,
    },

    /// Compare a file with its latest saved version
    Diff {
        /// File path to compare
        path: String,
    },

    /// Bring back an older version of a file
    Restore {
        /// File path to restore
        path: String,
        /// How far back to restore from (e.g. 10m, 1h)
        duration: String,
    },

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
