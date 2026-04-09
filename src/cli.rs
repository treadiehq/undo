use clap::{Parser, Subcommand};

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
    Start,

    /// Show recent file activity
    Timeline {
        /// Maximum number of events to show
        #[arg(long, default_value = "20")]
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
    Stop,

    /// Update undo to the latest release
    Update,
}
