use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::path::Path;

mod activity;
mod ask;
mod cli;
mod daemon;
mod db;
mod diff;
mod duration;
mod groups;
mod ignore;
mod integrity;
mod logging;
mod models;
mod recover;
mod restore;
mod retention;
mod sessions;
mod snapshots;
mod update;
mod watcher;

// ── ANSI colors ─────────────────────────────────────────────────────

pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RESET: &str = "\x1b[0m";

// ── helpers ─────────────────────────────────────────────────────────

// ── test isolation ───────────────────────────────────────────────────
//
// Tests that exercise code paths touching `backtrack_dir()` (snapshots,
// retention, watcher) would otherwise write into the real ~/.undo directory.
// Setting a thread-local override redirects all such writes to a tempdir,
// giving each test its own isolated storage that is cleaned up on drop.

#[cfg(test)]
thread_local! {
    static TEST_DATA_DIR: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Redirect `backtrack_dir()` to `path` for the duration of the current test.
/// Call at the top of any test that exercises snapshot or retention I/O.
#[cfg(test)]
pub fn set_test_data_dir(path: std::path::PathBuf) {
    TEST_DATA_DIR.with(|d| *d.borrow_mut() = Some(path));
}

pub fn backtrack_dir() -> Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    // In test builds, honour the per-thread override so snapshot and retention
    // I/O lands in a tempdir rather than ~/.undo.
    #[cfg(test)]
    {
        let test_dir = TEST_DATA_DIR.with(|d| d.borrow().clone());
        if let Some(dir) = test_dir {
            std::fs::create_dir_all(&dir)?;
            return Ok(dir);
        }
    }

    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".undo");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(&dir)?;
    builder.create(dir.join("snapshots"))?;
    builder.create(dir.join("pids"))?;
    Ok(dir)
}

/// Resolve a user-provided path and verify it stays within the project root.
/// Prevents path traversal via `../` and symlinks pointing outside the project.
pub fn safe_resolve_path(
    cwd: &Path,
    path_str: &str,
    project_root: &str,
) -> Result<std::path::PathBuf> {
    let abs_path = cwd.join(path_str);
    let resolved = if abs_path.exists() {
        abs_path.canonicalize()?
    } else {
        // For non-existent files, normalize manually. Iterate over the *joined*
        // path's components (not `path_str` alone) so absolute inputs like
        // "/abs/foo" — which `cwd.join` correctly produces as "/abs/foo" — are
        // handled. The previous implementation iterated `path_str.components()`
        // and silently dropped Component::RootDir, treating "/abs/foo" as if
        // it were "<cwd>/abs/foo".
        let mut normalized = std::path::PathBuf::new();
        for component in abs_path.components() {
            match component {
                std::path::Component::Prefix(p) => normalized.push(p.as_os_str()),
                std::path::Component::RootDir => normalized.push("/"),
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                std::path::Component::Normal(c) => normalized.push(c),
                std::path::Component::CurDir => {}
            }
        }

        // Defense against parent-directory symlinks: syntactic normalization
        // does NOT follow symlinks, so `<root>/sym/missing.txt` (where `sym`
        // is a symlink to `/etc`) passes the bounds check below even though
        // a subsequent `open()` would write to `/etc/missing.txt`. Resolve
        // the deepest existing ancestor with `canonicalize()` and re-attach
        // the missing tail so the bounds check sees the real location.
        //
        // Components are collected into a Vec and re-pushed at the end rather
        // than accumulated via `PathBuf::push` of an empty PathBuf, which can
        // append a stray separator on some platforms.
        let mut ancestor = normalized.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !ancestor.exists() {
            let Some(name) = ancestor.file_name().map(|n| n.to_os_string()) else {
                break;
            };
            tail.push(name);
            if !ancestor.pop() {
                break;
            }
        }
        if ancestor.as_os_str().is_empty() || !ancestor.exists() {
            normalized
        } else {
            let mut result = ancestor.canonicalize()?;
            for name in tail.iter().rev() {
                result.push(name);
            }
            result
        }
    };

    // Canonicalize the project root for the bounds check. The ancestor walk
    // above produces a path whose existing prefix has been resolved through
    // symlinks (e.g. on macOS `/var/...` becomes `/private/var/...`), so the
    // root we compare against must be canonicalized too — otherwise a
    // perfectly legitimate subpath inside the canonical-form root reads as
    // "outside" the literal-form root and gets rejected. In production the
    // stored `project.root_path` is already canonical (set via
    // `cwd.canonicalize()` in `cmd_start`), so this is a no-op there.
    let root_path = std::path::Path::new(project_root);
    let root_canonical = root_path
        .canonicalize()
        .unwrap_or_else(|_| root_path.to_path_buf());
    let resolved_str = resolved.to_string_lossy();
    let root_str = root_canonical.to_string_lossy();

    if !resolved_str.starts_with(root_str.as_ref())
        || (resolved_str.len() > root_str.len() && resolved_str.as_bytes()[root_str.len()] != b'/')
    {
        anyhow::bail!(
            "path '{}' resolves outside the project root ({})",
            path_str,
            project_root,
        );
    }

    Ok(resolved)
}

pub fn find_project(db: &db::Database, cwd: &Path) -> Result<models::WatchedProject> {
    db.find_project_for_path(cwd)?.ok_or_else(|| {
        anyhow::anyhow!("no project is being watched for this directory.\nRun `undo start` first.")
    })
}

pub fn relative_path<'a>(abs_path: &'a str, project_root: &str) -> &'a str {
    abs_path
        .strip_prefix(project_root)
        .and_then(|p| p.strip_prefix('/'))
        .unwrap_or(abs_path)
}

/// Lowercase-hex encode bytes into a single allocation. The previous
/// `bytes.iter().map(|b| format!("{:02x}", b)).collect()` idiom heap-allocated
/// a `String` per byte (32 allocations per SHA-256), which is wasteful on the
/// hashing hot path. A pre-sized buffer + nibble lookup avoids both the
/// per-byte allocation and the `format!` machinery.
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn event_color(event_type: &str) -> &'static str {
    match event_type {
        "MODIFIED" => YELLOW,
        "CREATED" => GREEN,
        "DELETED" => RED,
        "RENAMED" => BLUE,
        _ => "",
    }
}

// ── entry point ─────────────────────────────────────────────────────

fn main() {
    let cli = cli::Cli::parse();

    let result = match cli.command {
        cli::Command::Start { force } => daemon::cmd_start(cli.verbose, force),
        cli::Command::Timeline {
            limit,
            since,
            bursts,
            deleted,
        } => activity::cmd_timeline(limit, since.as_deref(), bursts, deleted),
        cli::Command::WhatChanged { duration } => cmd_what_changed(&duration),
        cli::Command::Diff {
            path,
            duration,
            checkpoint,
            summary,
            stat,
        } => diff::cmd_diff(
            &path,
            duration.as_deref(),
            checkpoint.as_deref(),
            summary,
            stat,
        ),
        cli::Command::Preview { path, duration } => restore::cmd_preview(&path, &duration),
        cli::Command::Restore {
            path,
            duration,
            preview,
            checkpoint,
            timestamp,
            deleted,
            yes,
        } => restore::cmd_restore(
            path.as_deref(),
            duration.as_deref(),
            checkpoint.as_deref(),
            timestamp,
            preview,
            deleted,
            yes,
        ),
        cli::Command::Checkpoint { name } => activity::cmd_checkpoint(&name),
        cli::Command::Checkpoints => activity::cmd_checkpoints(),
        cli::Command::Deleted { limit } => activity::cmd_deleted(limit),
        cli::Command::RestoreDeleted { path } => restore::cmd_restore_deleted(&path),
        cli::Command::Panic {
            restore_before_latest_burst,
            yes,
        } => activity::cmd_panic(restore_before_latest_burst, yes),
        cli::Command::Session { command } => match command {
            cli::SessionCommand::Start { name } => sessions::cmd_session_start(&name),
            cli::SessionCommand::Stop => sessions::cmd_session_stop(),
            cli::SessionCommand::Show { name } => sessions::cmd_session_show(&name),
        },
        cli::Command::Sessions => sessions::cmd_sessions(),
        cli::Command::Recover(args) => {
            recover::cmd_recover(&args.session, args.group.as_deref(), args.preview, args.yes)
        }
        cli::Command::Ask(args) => {
            ask::cmd_ask(&args.query, args.session.as_deref(), args.apply, args.yes)
        }
        cli::Command::Status => daemon::cmd_status(),
        cli::Command::Stop { all } => daemon::cmd_stop(all),
        cli::Command::Prune { keep, dry_run } => cmd_prune(keep, dry_run),
        cli::Command::Update => update::cmd_update(),
    };

    if let Err(e) = result {
        // Tees to stderr (unchanged UX) and, when the daemon logger is active,
        // records the failure in ~/.undo/undo.log so crashes are diagnosable.
        logging::error(&e.to_string());
        std::process::exit(1);
    }
}

// ── prune ────────────────────────────────────────────────────────────

fn cmd_prune(keep: Option<String>, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = db::Database::open()?;
    let project = find_project(&db, &cwd)?;

    // Load `.undorc` from the project *root*, not the cwd. `undo prune` is
    // commonly run from a subdirectory; passing `&cwd` here silently falls
    // back to defaults whenever the user isn't standing in the project root,
    // even though their `.undorc` lives next to the watched code.
    let project_root_path = std::path::Path::new(&project.root_path);
    let mut config = retention::load_config(Some(project_root_path));
    if let Some(ref keep_str) = keep {
        // Seconds-precise: `--keep=12h` must mean 12 hours, not "round up to 1 day".
        let secs = duration::parse_duration(keep_str)?;
        config.retention_secs_override = Some(secs as u64);
    }

    let label = if dry_run { "Would delete" } else { "Deleted" };
    let stats = retention::prune(&db, project.id, &config, dry_run)?;

    println!(
        "{} {} events, {} saved copies, {} backups.",
        label, stats.events_deleted, stats.snapshots_deleted, stats.backups_deleted,
    );

    let usage = retention::total_disk_usage()?;
    println!(
        "Freed {}. Current storage: {}.",
        retention::format_size(stats.bytes_freed),
        retention::format_size(usage),
    );

    Ok(())
}

// ── what-changed ────────────────────────────────────────────────────

fn cmd_what_changed(duration_str: &str) -> Result<()> {
    let secs = duration::parse_duration(duration_str)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = db::Database::open()?;
    let project = find_project(&db, &cwd)?;

    // saturating_sub: parse_duration accepts up to i64::MAX seconds, so a bare
    // `now - secs` underflows (debug panic / release wrap). Saturate instead.
    let since = chrono::Utc::now().timestamp().saturating_sub(secs);
    let events = db.get_events_since(project.id, since)?;

    if events.is_empty() {
        println!("No changes in the last {}.", duration_str);
        return Ok(());
    }

    // Keep only the most recent event type per path (events arrive newest-first).
    let mut latest: HashMap<String, String> = HashMap::new();
    for event in &events {
        latest
            .entry(event.path.clone())
            .or_insert_with(|| event.event_type.clone());
    }

    // Group paths by event type.
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (path, etype) in &latest {
        grouped.entry(etype.clone()).or_default().push(path.clone());
    }

    println!("{}Changes in last {}{}", BOLD, duration_str, RESET);
    println!();

    for etype in &["MODIFIED", "CREATED", "DELETED", "RENAMED"] {
        if let Some(paths) = grouped.get(*etype) {
            let color = event_color(etype);
            println!("{}{}{}", color, etype, RESET);
            let mut sorted = paths.clone();
            sorted.sort();
            for path in &sorted {
                println!("  - {}", relative_path(path, &project.root_path));
            }
            println!();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The project root and leading slash are stripped to yield a clean relative path.
    #[test]
    fn relative_path_strips_prefix_and_leading_slash() {
        assert_eq!(
            relative_path("/home/user/project/src/main.rs", "/home/user/project"),
            "src/main.rs"
        );
    }

    /// A path outside the project root is returned unchanged.
    #[test]
    fn relative_path_returns_original_when_no_prefix_match() {
        assert_eq!(
            relative_path("/other/file.rs", "/home/user/project"),
            "/other/file.rs"
        );
    }

    /// ../ components that escape the project root must be blocked to prevent path traversal.
    #[test]
    fn safe_resolve_path_rejects_traversal_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        // "../../etc/passwd" should escape the project root.
        let result = safe_resolve_path(dir.path(), "../../etc/passwd", root);
        assert!(result.is_err(), "path traversal must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("outside the project root"), "got: {}", msg);
    }

    /// A well-formed subpath within the root is accepted and resolved correctly.
    #[test]
    fn safe_resolve_path_allows_valid_subpath() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        // A simple nested path that stays within the root must succeed.
        let result = safe_resolve_path(dir.path(), "src/main.rs", root);
        assert!(result.is_ok(), "valid subpath must be accepted");
        let resolved = result.unwrap();
        // Compare against the canonicalized root: on macOS the system tmpdir
        // is `/var/folders/...` which is a symlink to `/private/var/folders/...`.
        // The fix for non-existent parent-symlink escapes resolves the deepest
        // existing ancestor through `canonicalize()`, so the returned path is
        // in canonical form and must be checked against the canonical root.
        let canonical_root = dir.path().canonicalize().unwrap();
        assert!(
            resolved.starts_with(&canonical_root),
            "resolved {:?} must live under canonical root {:?}",
            resolved,
            canonical_root
        );
    }

    /// A non-existent ABSOLUTE path outside the project root must be rejected.
    /// The previous normaliser dropped Component::RootDir, so "/etc/shadow"
    /// was silently rewritten to "<cwd>/etc/shadow" — which sometimes lay
    /// inside the project root and slipped past the bounds check.
    #[test]
    fn safe_resolve_path_rejects_nonexistent_absolute_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        // Use an absolute path that does not exist and lives well outside cwd.
        let target = "/nonexistent_absolute_path_outside/test_file.xyz";
        let result = safe_resolve_path(dir.path(), target, root);
        assert!(
            result.is_err(),
            "absolute non-existent path outside root must be rejected, got: {:?}",
            result
        );
    }

    /// A non-existent path whose *parent* is a symlink pointing outside the
    /// project root must be rejected. Syntactic normalization alone is not
    /// enough: `<root>/sym/missing.txt` (where `sym -> /tmp/...`) reads as
    /// "inside the root" but `open()` would follow the symlink and write to
    /// `/tmp/.../missing.txt`. The fix canonicalizes the deepest existing
    /// ancestor and re-attaches the tail so the bounds check sees the real
    /// destination.
    #[test]
    fn safe_resolve_path_rejects_nonexistent_path_through_parent_symlink() {
        use std::os::unix::fs::symlink;
        let root_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = root_dir.path().to_str().unwrap();

        // <root>/sym -> <outside>
        symlink(outside.path(), root_dir.path().join("sym")).unwrap();

        // The leaf does not exist on either side of the symlink. The old
        // implementation accepted this because syntactic normalization yields
        // "<root>/sym/missing.txt" — passes the prefix check.
        let result = safe_resolve_path(root_dir.path(), "sym/missing.txt", root);
        assert!(
            result.is_err(),
            "non-existent path through a parent symlink that escapes the root must be rejected, got: {:?}",
            result
        );
    }

    /// A non-existent absolute path *inside* the project root resolves correctly,
    /// landing under the canonical root rather than being re-anchored under cwd.
    /// (The old normaliser dropped `Component::RootDir` and produced
    /// "<cwd>/<root>/<path>".) The path is now also canonicalized through any
    /// ancestor symlinks (e.g. macOS `/var → /private/var`), so the
    /// expectation is built from the canonical root.
    #[test]
    fn safe_resolve_path_normalises_nonexistent_absolute_path_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root_path = dir.path();
        let root = root_path.to_str().unwrap();

        // Build an absolute path that's inside the root but doesn't exist on disk.
        let target_str = format!("{}/missing/child.rs", root);
        let resolved = safe_resolve_path(root_path, &target_str, root)
            .expect("absolute path inside root must be accepted");

        let canonical_root = root_path.canonicalize().unwrap();
        let expected = canonical_root.join("missing").join("child.rs");
        assert_eq!(
            resolved, expected,
            "absolute non-existent path must land under the canonical root, \
             not be re-anchored under cwd"
        );
    }

    /// Every event type maps to the expected ANSI colour; unknown types produce no colour code.
    #[test]
    fn event_color_maps_all_known_types() {
        assert_eq!(event_color("MODIFIED"), YELLOW);
        assert_eq!(event_color("CREATED"), GREEN);
        assert_eq!(event_color("DELETED"), RED);
        assert_eq!(event_color("RENAMED"), BLUE);
        // Unknown types should return an empty string (no color).
        assert_eq!(event_color("UNKNOWN"), "");
    }

    /// to_hex produces lowercase, zero-padded, two-chars-per-byte output —
    /// including a leading zero nibble, which is the easy thing to get wrong.
    #[test]
    fn to_hex_encodes_known_vectors() {
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x00]), "00");
        assert_eq!(to_hex(&[0x0f]), "0f");
        assert_eq!(to_hex(&[0xff]), "ff");
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    /// to_hex must be byte-for-byte identical to the previous
    /// `map(|b| format!("{:02x}", b)).collect()` idiom it replaced, across the
    /// full 0..=255 byte range.
    #[test]
    fn to_hex_matches_format_reference() {
        let all: Vec<u8> = (0..=255).collect();
        let reference: String = all.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(to_hex(&all), reference);
        assert_eq!(to_hex(&all).len(), all.len() * 2);
    }
}
