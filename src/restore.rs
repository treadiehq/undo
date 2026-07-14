use crate::db::Database;
use crate::duration;
use crate::models::{FileEvent, Session, WatchedProject};
use crate::snapshots;
use crate::{BOLD, GREEN, RED, RESET, YELLOW, find_project};
use anyhow::Result;
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn cmd_restore(
    path: Option<&str>,
    duration: Option<&str>,
    checkpoint: Option<&str>,
    timestamp: Option<i64>,
    preview: bool,
    deleted: bool,
    yes: bool,
) -> Result<()> {
    if deleted {
        let path = path.ok_or_else(|| anyhow::anyhow!("restore --deleted requires a path"))?;
        if duration.is_some() || checkpoint.is_some() || timestamp.is_some() {
            anyhow::bail!(
                "restore --deleted cannot be combined with a duration, --checkpoint, or --timestamp"
            );
        }
        return restore_deleted(path, preview);
    }

    let path = path.ok_or_else(|| anyhow::anyhow!("restore requires a path"))?;
    let (target_time, label) = resolve_restore_time(duration, checkpoint, timestamp)?;
    restore_at_timestamp(path, target_time, &label, preview, yes)
}

pub fn cmd_preview(path_str: &str, duration_str: &str) -> Result<()> {
    let secs = duration::parse_duration(duration_str)?;
    let target_time = Utc::now().timestamp().saturating_sub(secs);
    let label = format!("{} ago", duration_str);
    restore_at_timestamp(path_str, target_time, &label, true, false)
}

pub fn cmd_restore_deleted(path_str: &str) -> Result<()> {
    restore_deleted(path_str, false)
}

pub fn restore_at_timestamp(
    path_str: &str,
    target_time: i64,
    label: &str,
    preview: bool,
    yes: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;

    let plan = plan_restore(&db, &project, &cwd, path_str, target_time, true)?;

    if plan.entries.is_empty() {
        println!("No saved versions found for this restore target.");
        return Ok(());
    }

    if preview {
        print_restore_plan(&project, &plan, label)?;
        return Ok(());
    }

    apply_restore_plan(&project, &plan, yes)
}

pub(crate) fn restore_paths_at_session_start(
    paths: &[String],
    session: &Session,
    label: &str,
    preview: bool,
    yes: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let plan = plan_paths_restore_at_session_start(&db, &project, &cwd, paths, session)?;

    if plan.entries.is_empty() {
        println!("No saved versions found for this recovery target.");
        return Ok(());
    }

    if preview {
        print_restore_plan(&project, &plan, label)?;
        return Ok(());
    }

    apply_restore_plan(&project, &plan, yes)
}

fn resolve_restore_time(
    duration: Option<&str>,
    checkpoint: Option<&str>,
    timestamp: Option<i64>,
) -> Result<(i64, String)> {
    match (duration, checkpoint, timestamp) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            anyhow::bail!("use only one restore target: a duration, --checkpoint, or --timestamp")
        }
        (Some(duration), None, None) => {
            let secs = duration::parse_duration(duration)?;
            let target_time = Utc::now().timestamp().saturating_sub(secs);
            Ok((target_time, format!("{} ago", duration)))
        }
        (None, Some(name), None) => {
            let cwd = std::env::current_dir()?.canonicalize()?;
            let db = Database::open()?;
            let project = find_project(&db, &cwd)?;
            let checkpoint = db
                .get_checkpoint(project.id, name)?
                .ok_or_else(|| anyhow::anyhow!("checkpoint '{}' not found", name))?;
            Ok((checkpoint.timestamp, format!("checkpoint '{}'", name)))
        }
        (None, None, Some(timestamp)) => Ok((timestamp, format!("Unix timestamp {}", timestamp))),
        (None, None, None) => {
            anyhow::bail!("restore requires a duration, --checkpoint, or --timestamp")
        }
    }
}

fn restore_deleted(path_str: &str, preview: bool) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;

    let raw_path = cwd.join(path_str);
    reject_raw_symlink(&raw_path, path_str)?;

    let abs_path = crate::safe_resolve_path(&cwd, path_str, &project.root_path)?;
    let abs_path_str = abs_path.to_string_lossy().to_string();

    let Some(event) = db.get_latest_deleted_event(project.id, &abs_path_str)? else {
        println!("No recoverable deleted version found for this file.");
        return Ok(());
    };
    let Some(hash) = event.previous_hash else {
        println!("No recoverable deleted version found for this file.");
        return Ok(());
    };

    let source = RestoreSource {
        hash,
        timestamp: event.timestamp,
        kind: RestoreKind::DeletedFallback,
    };
    let entry = RestorePlanEntry {
        path: abs_path_str.clone(),
        rel_path: crate::relative_path(&abs_path_str, &project.root_path).to_string(),
        action: RestoreAction::Write { source },
    };
    let plan = RestorePlan {
        entries: vec![entry],
    };

    if preview {
        print_restore_plan(&project, &plan, "deleted version")?;
    } else {
        apply_restore_plan(&project, &plan, true)?;
    }
    Ok(())
}

fn reject_raw_symlink(raw_path: &Path, path_str: &str) -> Result<()> {
    if let Ok(meta) = raw_path.symlink_metadata()
        && meta.file_type().is_symlink()
    {
        anyhow::bail!("refusing to restore through symlink '{}'", path_str);
    }
    Ok(())
}

#[derive(Debug)]
struct RestorePlan {
    entries: Vec<RestorePlanEntry>,
}

#[derive(Debug)]
struct RestorePlanEntry {
    path: String,
    rel_path: String,
    action: RestoreAction,
}

#[derive(Debug)]
enum RestoreAction {
    Write { source: RestoreSource },
    DeleteCreatedAfterTarget,
}

fn plan_restore(
    db: &Database,
    project: &WatchedProject,
    cwd: &Path,
    path_str: &str,
    target_time: i64,
    allow_single_fallbacks: bool,
) -> Result<RestorePlan> {
    let raw_path = cwd.join(path_str);
    reject_raw_symlink(&raw_path, path_str)?;

    let abs_path = crate::safe_resolve_path(cwd, path_str, &project.root_path)?;
    let is_directory_scope = abs_path.is_dir() || path_str == ".";
    let mut entries = if is_directory_scope {
        plan_directory_restore(db, project, &abs_path, target_time)?
    } else if !abs_path.exists() {
        // A deleted path no longer carries filesystem type information. Try
        // scoped recovery first so tracked descendants (including old paths
        // from renames) can identify a deleted directory, while preserving
        // single-file fallbacks when the scoped plan has nothing to restore.
        let directory_entries = plan_directory_restore(db, project, &abs_path, target_time)?;
        if directory_entries.is_empty() {
            plan_single_file_restore(db, project, &abs_path, target_time, allow_single_fallbacks)?
        } else {
            directory_entries
        }
    } else {
        plan_single_file_restore(db, project, &abs_path, target_time, allow_single_fallbacks)?
    };

    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    Ok(RestorePlan { entries })
}

fn plan_single_file_restore(
    db: &Database,
    project: &WatchedProject,
    abs_path: &Path,
    target_time: i64,
    allow_fallbacks: bool,
) -> Result<Vec<RestorePlanEntry>> {
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let source = if allow_fallbacks {
        resolve_restore_source(db, project.id, &abs_path_str, target_time)?
    } else {
        resolve_exact_source_at_time(db, project.id, &abs_path_str, target_time)?
    };

    let Some(source) = source else {
        return Ok(Vec::new());
    };

    Ok(vec![RestorePlanEntry {
        rel_path: crate::relative_path(&abs_path_str, &project.root_path).to_string(),
        path: abs_path_str,
        action: RestoreAction::Write { source },
    }])
}

fn plan_directory_restore(
    db: &Database,
    project: &WatchedProject,
    abs_scope: &Path,
    target_time: i64,
) -> Result<Vec<RestorePlanEntry>> {
    let scope = abs_scope.to_string_lossy().to_string();
    let events = db.get_events_since(project.id, target_time)?;
    let mut paths = std::collections::BTreeSet::new();

    for event in events {
        if path_in_scope(&event.path, &scope) {
            paths.insert(event.path);
        }
        if let Some(old_path) = event.old_path
            && path_in_scope(&old_path, &scope)
        {
            paths.insert(old_path);
        }
    }

    let mut entries = Vec::new();
    for path in paths {
        let rel_path = crate::relative_path(&path, &project.root_path).to_string();
        if let Some(source) = resolve_exact_source_at_time(db, project.id, &path, target_time)? {
            entries.push(RestorePlanEntry {
                path,
                rel_path,
                action: RestoreAction::Write { source },
            });
        } else if Path::new(&path).exists() {
            entries.push(RestorePlanEntry {
                path,
                rel_path,
                action: RestoreAction::DeleteCreatedAfterTarget,
            });
        }
    }

    Ok(entries)
}

fn plan_paths_restore_at_session_start(
    db: &Database,
    project: &WatchedProject,
    cwd: &Path,
    paths: &[String],
    session: &Session,
) -> Result<RestorePlan> {
    let mut entries = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for path_str in paths {
        let raw_path = cwd.join(path_str);
        reject_raw_symlink(&raw_path, path_str)?;
        let abs_path = crate::safe_resolve_path(cwd, path_str, &project.root_path)?;
        let abs_path_str = abs_path.to_string_lossy().to_string();
        if !seen.insert(abs_path_str.clone()) {
            continue;
        }
        let rel_path = crate::relative_path(&abs_path_str, &project.root_path).to_string();
        if let Some(source) = resolve_exact_source_at_session_start(db, session, &abs_path_str)? {
            entries.push(RestorePlanEntry {
                path: abs_path_str,
                rel_path,
                action: RestoreAction::Write { source },
            });
        } else if abs_path.exists() {
            entries.push(RestorePlanEntry {
                path: abs_path_str,
                rel_path,
                action: RestoreAction::DeleteCreatedAfterTarget,
            });
        }
    }

    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(RestorePlan { entries })
}

fn path_in_scope(path: &str, scope: &str) -> bool {
    path == scope
        || path
            .strip_prefix(scope)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn print_restore_plan(project: &WatchedProject, plan: &RestorePlan, label: &str) -> Result<()> {
    let writes = plan
        .entries
        .iter()
        .filter(|e| matches!(e.action, RestoreAction::Write { .. }))
        .count();
    let deletes = plan.entries.len() - writes;

    println!(
        "{}Preview restore{} to {} ({} write(s), {} delete(s))",
        BOLD, RESET, label, writes, deletes
    );
    println!();

    for entry in &plan.entries {
        match &entry.action {
            RestoreAction::Write { source } => {
                let age = Utc::now().timestamp() - source.timestamp;
                println!(
                    "{}Would restore{} {} from {}.",
                    GREEN,
                    RESET,
                    entry.rel_path,
                    duration::format_elapsed(age)
                );
                print_entry_preview(project, entry, source)?;
            }
            RestoreAction::DeleteCreatedAfterTarget => {
                println!(
                    "{}Would delete{} {} (created after restore target).",
                    RED, RESET, entry.rel_path
                );
                print_delete_preview(entry)?;
            }
        }
        println!();
    }

    Ok(())
}

fn print_entry_preview(
    project: &WatchedProject,
    entry: &RestorePlanEntry,
    source: &RestoreSource,
) -> Result<()> {
    let restored = snapshots::load(project.id, &source.hash)?;
    let path = Path::new(&entry.path);
    if !path.exists() {
        println!("  File does not exist now; restore would recreate it.");
        return Ok(());
    }
    let Some(current) = crate::diff::read_capped(path, snapshots::MAX_SNAPSHOT_SIZE)? else {
        println!("  Current file is too large to preview.");
        return Ok(());
    };
    crate::diff::print_bytes_diff(&current, &restored, &entry.rel_path, "current", "restored")?;
    Ok(())
}

fn print_delete_preview(entry: &RestorePlanEntry) -> Result<()> {
    let path = Path::new(&entry.path);
    if !path.exists() {
        println!("  File is already absent.");
        return Ok(());
    }
    let Some(current) = crate::diff::read_capped(path, snapshots::MAX_SNAPSHOT_SIZE)? else {
        println!("  Current file is too large to preview.");
        return Ok(());
    };
    crate::diff::print_bytes_diff(&current, b"", &entry.rel_path, "current", "deleted")?;
    Ok(())
}

fn apply_restore_plan(project: &WatchedProject, plan: &RestorePlan, yes: bool) -> Result<()> {
    if plan.entries.len() > 1 && !yes {
        anyhow::bail!(
            "restore would change {} files. Run with --preview first, then pass --yes to apply.",
            plan.entries.len()
        );
    }

    for entry in &plan.entries {
        match &entry.action {
            RestoreAction::Write { source } => apply_write(project, entry, source)?,
            RestoreAction::DeleteCreatedAfterTarget => apply_delete(entry)?,
        }
    }

    Ok(())
}

fn apply_write(
    project: &WatchedProject,
    entry: &RestorePlanEntry,
    source: &RestoreSource,
) -> Result<()> {
    let abs_path = Path::new(&entry.path);
    reject_raw_symlink(abs_path, &entry.rel_path)?;
    let content = snapshots::load(project.id, &source.hash)?;

    match source.kind {
        RestoreKind::Exact => {}
        RestoreKind::OldestFallback => {
            let age = Utc::now().timestamp() - source.timestamp;
            println!(
                "No saved version that far back. Using the earliest one available (from {}).",
                duration::format_elapsed(age)
            );
        }
        RestoreKind::DeletedFallback => {
            let age = Utc::now().timestamp() - source.timestamp;
            println!(
                "File was deleted {} — restoring its last recorded contents.",
                duration::format_elapsed(age)
            );
        }
    }

    if abs_path.exists() {
        let backup_path = create_restore_backup(abs_path)?;
        println!("Backup of current file saved to {}", backup_path.display());
    }

    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    write_restore_atomically(abs_path, &content)?;

    let elapsed = Utc::now().timestamp() - source.timestamp;
    let ago = duration::format_elapsed(elapsed);

    println!(
        "{}Restored{} {} from the version saved {}.",
        GREEN, RESET, entry.rel_path, ago
    );

    Ok(())
}

fn apply_delete(entry: &RestorePlanEntry) -> Result<()> {
    let abs_path = Path::new(&entry.path);
    reject_raw_symlink(abs_path, &entry.rel_path)?;
    if !abs_path.exists() {
        return Ok(());
    }
    let backup_path = create_restore_backup(abs_path)?;
    std::fs::remove_file(abs_path)?;
    println!(
        "{}Deleted{} {} and saved a backup to {}.",
        YELLOW,
        RESET,
        entry.rel_path,
        backup_path.display()
    );
    Ok(())
}

/// Which snapshot content `restore` should write, and where it came from.
#[derive(Debug, Clone)]
pub(crate) struct RestoreSource {
    hash: String,
    timestamp: i64,
    kind: RestoreKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RestoreKind {
    /// A snapshot at or before the requested time.
    Exact,
    /// No snapshot that far back; using the earliest available instead.
    OldestFallback,
    /// The file is gone; recovering its last contents from the DELETED event.
    DeletedFallback,
}

/// Decide which snapshot to restore for `path` as of `target_time`.
///
/// Resolution order:
/// 1. the newest non-DELETE snapshot at or before `target_time`,
/// 2. the state immediately before the first later change, when older history
///    was pruned but that event's `previous_hash` still proves the path existed,
/// 3. the earliest recorded snapshot (when the window predates all history),
/// 4. the last contents of a deleted file, recovered from the most recent
///    DELETED event's `previous_hash`.
///
/// Step 4 is the fix for files that were deleted after their creating event
/// aged out of retention: previously both earlier lookups returned `None` and
/// restore reported "No snapshots found", even though the deletion itself was
/// well within the retention window and the snapshot was still on disk.
fn resolve_restore_source(
    db: &Database,
    project_id: i64,
    path: &str,
    target_time: i64,
) -> Result<Option<RestoreSource>> {
    if let Some(source) = resolve_exact_source_at_time(db, project_id, path, target_time)? {
        return Ok(Some(source));
    }

    if let Some(e) = db.get_oldest_event(project_id, path)?
        && let Some(hash) = e.current_hash
    {
        return Ok(Some(RestoreSource {
            hash,
            timestamp: e.timestamp,
            kind: RestoreKind::OldestFallback,
        }));
    }

    if let Some(e) = db.get_latest_deleted_event(project_id, path)?
        && let Some(hash) = e.previous_hash
    {
        return Ok(Some(RestoreSource {
            hash,
            timestamp: e.timestamp,
            kind: RestoreKind::DeletedFallback,
        }));
    }

    Ok(None)
}

fn source_before_event(event: FileEvent, path: &str) -> Option<RestoreSource> {
    let path_was_present = event.old_path.as_deref() == Some(path)
        || (event.path == path && matches!(event.event_type.as_str(), "MODIFIED" | "DELETED"));
    if !path_was_present {
        return None;
    }

    Some(RestoreSource {
        hash: event.previous_hash?,
        timestamp: event.timestamp,
        kind: RestoreKind::Exact,
    })
}

fn resolve_exact_source_at_time(
    db: &Database,
    project_id: i64,
    path: &str,
    target_time: i64,
) -> Result<Option<RestoreSource>> {
    if let Some(e) = db.get_event_at_time(project_id, path, target_time)?
        && let Some(hash) = e.current_hash
    {
        return Ok(Some(RestoreSource {
            hash,
            timestamp: e.timestamp,
            kind: RestoreKind::Exact,
        }));
    }

    if let Some(event) = db.get_first_path_event_after(project_id, path, target_time)? {
        return Ok(source_before_event(event, path));
    }

    Ok(None)
}

fn resolve_exact_source_at_session_start(
    db: &Database,
    session: &Session,
    path: &str,
) -> Result<Option<RestoreSource>> {
    if let Some(e) = db.get_event_at_session_start(session, path)?
        && let Some(hash) = e.current_hash
    {
        return Ok(Some(RestoreSource {
            hash,
            timestamp: e.timestamp,
            kind: RestoreKind::Exact,
        }));
    }

    if let Some(event) = db.get_first_path_event_in_session(session, path)? {
        return Ok(source_before_event(event, path));
    }

    Ok(None)
}

fn backup_timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn backup_path_hash(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let digest = Sha256::digest(path.as_os_str().as_bytes());
    crate::to_hex(&digest[..8])
}

fn backup_path_for(
    abs_path: &Path,
    backups_dir: &Path,
    timestamp_nanos: u128,
    attempt: usize,
) -> PathBuf {
    let filename = abs_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let path_hash = backup_path_hash(abs_path);
    let retry_suffix = if attempt == 0 {
        String::new()
    } else {
        format!(".{}", attempt)
    };
    backups_dir.join(format!(
        "{}_{}_{}{}.bak",
        filename, path_hash, timestamp_nanos, retry_suffix
    ))
}

fn create_restore_backup(abs_path: &Path) -> Result<PathBuf> {
    create_restore_backup_at(abs_path, backup_timestamp_nanos())
}

fn create_restore_backup_at(abs_path: &Path, timestamp_nanos: u128) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let backups_dir = crate::backtrack_dir()?.join("backups");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&backups_dir)?;
    // Restrict backups dir to owner-only.
    let _ = std::fs::set_permissions(&backups_dir, std::fs::Permissions::from_mode(0o700));

    for attempt in 0..1000 {
        let backup_path = backup_path_for(abs_path, &backups_dir, timestamp_nanos, attempt);
        let open_result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backup_path);

        let mut backup = match open_result {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        };

        let mut current = std::fs::File::open(abs_path)?;
        std::io::copy(&mut current, &mut backup)?;
        backup.sync_all()?;
        let _ = std::fs::set_permissions(&backup_path, std::fs::Permissions::from_mode(0o600));
        return Ok(backup_path);
    }

    anyhow::bail!(
        "could not create a unique restore backup for {}",
        abs_path.display()
    );
}

/// Unique sibling path for the restore temp file — always distinct from `target`,
/// including when `target` uses `.undo_tmp` or similar as its extension.
fn restore_atomic_temp_path(target: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut s = target.as_os_str().to_os_string();
    s.push(format!(".undo.partial.{}_{}", std::process::id(), nanos));
    PathBuf::from(s)
}

/// Same pattern as `snapshots::save_in`: `create_new` temp, full write, `rename` into place.
fn write_restore_atomically(target: &Path, content: &[u8]) -> Result<()> {
    let tmp_path = restore_atomic_temp_path(target);
    let _ = std::fs::remove_file(&tmp_path);

    let write_result = (|| -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
        std::fs::rename(&tmp_path, target)?;
        // Persist the rename itself: fsync the parent directory so the restored
        // file's dirent survives power loss, matching the durable snapshot write.
        // Best-effort — some filesystems reject directory fsync, and a restore that
        // otherwise succeeded should not fail on it.
        if let Some(parent) = target.parent() {
            let _ = crate::snapshots::fsync_dir(parent);
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    write_result.map_err(|e| e.into())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    /// Lock in the discipline that the symlink guard must see the *unresolved*
    /// path. `safe_resolve_path` calls `canonicalize()`, which follows symlinks,
    /// so checking the resolved path makes the guard dead code. This test
    /// proves the bug pattern: canonicalize hides the symlink, but
    /// symlink_metadata on the raw path catches it.
    #[test]
    fn symlink_guard_must_inspect_unresolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, "real content").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap();

        // The canonical (resolved) path is the real file — never a symlink.
        let canon = link.canonicalize().unwrap();
        let canon_is_symlink = canon
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        assert!(
            !canon_is_symlink,
            "canonicalize() must follow the symlink — proves the post-resolve check is dead code"
        );

        // The raw (unresolved) path IS a symlink — this is what the fix inspects.
        let raw_is_symlink = link
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        assert!(
            raw_is_symlink,
            "raw symlink_metadata() must report the link as a symlink"
        );
    }

    /// A regular file is not flagged by the unresolved-path symlink check.
    #[test]
    fn symlink_guard_accepts_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("plain.txt");
        std::fs::write(&regular, "hi").unwrap();
        let is_symlink = regular
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        assert!(!is_symlink);
    }

    use super::{
        RestoreAction, RestoreKind, backup_path_for, create_restore_backup_at, path_in_scope,
        plan_paths_restore_at_session_start, plan_restore, resolve_exact_source_at_time,
        resolve_restore_source, resolve_restore_time,
    };
    use crate::db::Database;

    fn mem_db() -> (Database, i64) {
        let db = Database::open_in_memory().unwrap();
        let p = db
            .get_or_create_project(std::path::Path::new("/proj"))
            .unwrap();
        (db, p.id)
    }

    #[test]
    fn absolute_timestamp_restore_target_is_stable() {
        let (target, label) = resolve_restore_time(None, None, Some(1_713_200_000)).unwrap();

        assert_eq!(target, 1_713_200_000);
        assert_eq!(label, "Unix timestamp 1713200000");
    }

    /// Same-basename files in different directories must not collapse to the
    /// same backup name when restored in the same timestamp window.
    #[test]
    fn backup_path_includes_full_path_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("backups");
        let first = dir.path().join("pkg-a/config.json");
        let second = dir.path().join("pkg-b/config.json");

        let first_backup = backup_path_for(&first, &backups, 123, 0);
        let second_backup = backup_path_for(&second, &backups, 123, 0);

        assert_ne!(
            first_backup, second_backup,
            "same basename and timestamp must still produce distinct backups"
        );
        assert!(
            first_backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("config.json_"),
            "backup name should remain recognizable"
        );
    }

    /// Even if the exact computed name already exists, creating a backup must
    /// allocate a fresh path instead of overwriting the earlier safety copy.
    #[test]
    fn create_restore_backup_never_overwrites_existing_backup() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let tree = tempfile::tempdir().unwrap();
        let file = tree.path().join("pkg/config.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "first state").unwrap();

        let first_backup = create_restore_backup_at(&file, 123).unwrap();
        std::fs::write(&file, "second state").unwrap();
        let second_backup = create_restore_backup_at(&file, 123).unwrap();

        assert_ne!(first_backup, second_backup);
        assert_eq!(
            std::fs::read_to_string(first_backup).unwrap(),
            "first state"
        );
        assert_eq!(
            std::fs::read_to_string(second_backup).unwrap(),
            "second state"
        );
    }

    /// A file deleted after its creating event aged out of retention must still
    /// be recoverable from the DELETED event's previous_hash. This is the bug:
    /// `get_event_at_time` and `get_oldest_event` both exclude DELETED rows, so
    /// before the fix restore gave up with "No snapshots found" even though the
    /// deletion was recent. The asserts below first prove both prior lookups
    /// come up empty (the red condition), then prove the resolver recovers it.
    #[test]
    fn resolve_recovers_deleted_file_from_previous_hash() {
        let (db, pid) = mem_db();
        let path = "/proj/gone.rs";
        let now = chrono::Utc::now().timestamp();

        // Only a DELETED event remains (creating/modify events already pruned).
        db.insert_event(
            pid,
            path,
            "DELETED",
            None,
            Some("last_hash"),
            None,
            None,
            None,
        )
        .unwrap();

        // Both pre-existing lookups return nothing — what the old code saw.
        assert!(db.get_event_at_time(pid, path, now).unwrap().is_none());
        assert!(db.get_oldest_event(pid, path).unwrap().is_none());

        // The resolver recovers the last contents via the deleted fallback.
        let src = resolve_restore_source(&db, pid, path, now)
            .unwrap()
            .expect("deleted file must be recoverable from previous_hash");
        assert_eq!(src.hash, "last_hash");
        assert_eq!(src.kind, RestoreKind::DeletedFallback);
    }

    /// When a normal snapshot exists at or before the target time, the resolver
    /// returns it as an Exact match — the deleted-file fallback must not change
    /// the happy path.
    #[test]
    fn resolve_prefers_exact_snapshot_over_fallbacks() {
        let (db, pid) = mem_db();
        let path = "/proj/live.rs";
        db.insert_event(
            pid,
            path,
            "MODIFIED",
            Some("good_hash"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        // Query with a comfortably-future target so the event (timestamped at
        // insert time) is unambiguously "at or before" it.
        let target = chrono::Utc::now().timestamp() + 3600;

        let src = resolve_restore_source(&db, pid, path, target)
            .unwrap()
            .unwrap();
        assert_eq!(src.hash, "good_hash");
        assert_eq!(src.kind, RestoreKind::Exact);
    }

    /// A path with no events at all yields None so the caller can report that
    /// nothing is recoverable.
    #[test]
    fn resolve_returns_none_when_nothing_recorded() {
        let (db, pid) = mem_db();
        let now = chrono::Utc::now().timestamp();
        assert!(
            resolve_restore_source(&db, pid, "/proj/never.rs", now)
                .unwrap()
                .is_none()
        );
    }

    /// Project restore needs exact point-in-time semantics: a file created after
    /// the target must be deleted, not restored via the single-file oldest fallback.
    #[test]
    fn exact_resolver_does_not_use_oldest_fallback() {
        let (db, pid) = mem_db();
        let path = "/proj/new.rs";
        db.insert_event(
            pid,
            path,
            "CREATED",
            Some("new_hash"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let before_creation = chrono::Utc::now().timestamp() - 3600;

        assert!(
            resolve_exact_source_at_time(&db, pid, path, before_creation)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            resolve_restore_source(&db, pid, path, before_creation)
                .unwrap()
                .unwrap()
                .kind,
            RestoreKind::OldestFallback
        );
    }

    /// When older source-path history has been pruned, the first rename after
    /// the target still carries the source contents in `previous_hash`.
    #[test]
    fn exact_resolver_recovers_rename_source_from_previous_hash() {
        let (db, pid) = mem_db();
        let old_path = "/proj/src/auth/login.rs";
        let new_path = "/proj/src/billing/signin.rs";
        let target_before_rename = chrono::Utc::now().timestamp().saturating_sub(60);
        db.insert_event(
            pid,
            new_path,
            "RENAMED",
            Some("current_hash"),
            Some("source_hash"),
            None,
            Some(old_path),
            Some(16),
        )
        .unwrap();

        let source =
            resolve_exact_source_at_time(&db, pid, old_path, target_before_rename).unwrap();

        assert_eq!(source.unwrap().hash, "source_hash");
    }

    /// A later rename does not prove its source path existed at the target when
    /// the first event after that target created the source path.
    #[test]
    fn exact_resolver_does_not_restore_path_created_after_target() {
        let (db, pid) = mem_db();
        let old_path = "/proj/src/generated.rs";
        let new_path = "/proj/archive/generated.rs";
        let target_before_creation = chrono::Utc::now().timestamp().saturating_sub(60);
        db.insert_event(
            pid,
            old_path,
            "CREATED",
            Some("generated_hash"),
            None,
            None,
            None,
            Some(16),
        )
        .unwrap();
        db.insert_event(
            pid,
            new_path,
            "RENAMED",
            Some("generated_hash"),
            Some("generated_hash"),
            None,
            Some(old_path),
            Some(16),
        )
        .unwrap();

        assert!(
            resolve_exact_source_at_time(&db, pid, old_path, target_before_creation)
                .unwrap()
                .is_none()
        );
    }

    /// Scope checks must treat sibling string prefixes as outside the directory.
    #[test]
    fn path_in_scope_rejects_shared_prefix_siblings() {
        assert!(path_in_scope("/proj/src/main.rs", "/proj/src"));
        assert!(path_in_scope("/proj/src", "/proj/src"));
        assert!(!path_in_scope("/proj/src-old/main.rs", "/proj/src"));
    }

    /// Checkpoint-style restore planning should write the exact source available
    /// at the target timestamp for a tracked file.
    #[test]
    fn plan_restore_writes_exact_source_for_file_target() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("tracked.rs");
        std::fs::write(&file, "current").unwrap();
        let file_str = file.to_string_lossy().to_string();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        db.insert_event(
            project.id,
            &file_str,
            "MODIFIED",
            Some("saved_hash"),
            None,
            None,
            None,
            Some(7),
        )
        .unwrap();

        let target_after_event = chrono::Utc::now().timestamp() + 60;
        let plan =
            plan_restore(&db, &project, &root, "tracked.rs", target_after_event, true).unwrap();

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].rel_path, "tracked.rs");
        match &plan.entries[0].action {
            RestoreAction::Write { source } => {
                assert_eq!(source.hash, "saved_hash");
                assert_eq!(source.kind, RestoreKind::Exact);
            }
            other => panic!("expected write action, got {other:?}"),
        }
    }

    /// A deleted directory has no filesystem type, so restore planning must
    /// discover its tracked children instead of treating its path as a file.
    #[test]
    fn plan_restore_finds_files_in_deleted_directory() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        let file = src_dir.join("main.rs");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap();
        let file_str = file.to_string_lossy().to_string();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        db.insert_event(
            project.id,
            &file_str,
            "MODIFIED",
            Some("saved_hash"),
            None,
            None,
            None,
            Some(12),
        )
        .unwrap();
        let target_before_deletion = chrono::Utc::now().timestamp();
        db.insert_event(
            project.id,
            &file_str,
            "DELETED",
            None,
            Some("saved_hash"),
            None,
            None,
            Some(12),
        )
        .unwrap();

        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(&src_dir).unwrap();

        let plan = plan_restore(&db, &project, &root, "src", target_before_deletion, true).unwrap();

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].rel_path, "src/main.rs");
        match &plan.entries[0].action {
            RestoreAction::Write { source } => {
                assert_eq!(source.hash, "saved_hash");
                assert_eq!(source.kind, RestoreKind::Exact);
            }
            other => panic!("expected write action, got {other:?}"),
        }
    }

    /// A directory rollback to before a generated file existed should plan a
    /// delete, not resurrect that file via the single-file oldest fallback.
    #[test]
    fn plan_restore_deletes_files_created_after_target() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let generated = root.join("generated.rs");
        std::fs::write(&generated, "agent output").unwrap();
        let generated_str = generated.to_string_lossy().to_string();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        db.insert_event(
            project.id,
            &generated_str,
            "CREATED",
            Some("generated_hash"),
            None,
            None,
            None,
            Some(12),
        )
        .unwrap();

        let target_before_event = chrono::Utc::now().timestamp().saturating_sub(60);
        let plan = plan_restore(&db, &project, &root, ".", target_before_event, true).unwrap();

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].rel_path, "generated.rs");
        assert!(matches!(
            plan.entries[0].action,
            RestoreAction::DeleteCreatedAfterTarget
        ));
    }

    /// Rolling back a cross-directory rename writes the original source path
    /// and removes the destination, even if only the rename event remains.
    #[test]
    fn plan_directory_restore_reverses_cross_directory_rename() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let old_path = root.join("src/auth/login.rs");
        let new_path = root.join("src/billing/signin.rs");
        std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        std::fs::write(&new_path, "original content").unwrap();
        let old_str = old_path.to_string_lossy().to_string();
        let new_str = new_path.to_string_lossy().to_string();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        let target_before_rename = chrono::Utc::now().timestamp().saturating_sub(60);
        db.insert_event(
            project.id,
            &new_str,
            "RENAMED",
            Some("current_hash"),
            Some("source_hash"),
            None,
            Some(&old_str),
            Some(16),
        )
        .unwrap();

        let plan = plan_restore(&db, &project, &root, ".", target_before_rename, true).unwrap();
        let old_entry = plan
            .entries
            .iter()
            .find(|entry| entry.path == old_str)
            .expect("the rename source should be restored");
        let new_entry = plan
            .entries
            .iter()
            .find(|entry| entry.path == new_str)
            .expect("the rename destination should be removed");

        match &old_entry.action {
            RestoreAction::Write { source } => assert_eq!(source.hash, "source_hash"),
            other => panic!("expected source write, got {other:?}"),
        }
        assert!(matches!(
            new_entry.action,
            RestoreAction::DeleteCreatedAfterTarget
        ));
    }

    /// Session recovery uses the rename event itself when the source path's
    /// older event has already aged out of retention.
    #[test]
    fn plan_session_restore_reverses_rename_with_pruned_source_history() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let old_path = root.join("src/auth/login.rs");
        let new_path = root.join("src/billing/signin.rs");
        std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        std::fs::write(&new_path, "original content").unwrap();
        let old_str = old_path.to_string_lossy().to_string();
        let new_str = new_path.to_string_lossy().to_string();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        db.start_session(project.id, "rename-run", "manual")
            .unwrap();
        db.insert_event(
            project.id,
            &new_str,
            "RENAMED",
            Some("current_hash"),
            Some("source_hash"),
            None,
            Some(&old_str),
            Some(16),
        )
        .unwrap();
        let session = db.stop_active_session(project.id).unwrap().unwrap();

        let plan = plan_paths_restore_at_session_start(
            &db,
            &project,
            &root,
            &[old_str.clone(), new_str.clone()],
            &session,
        )
        .unwrap();

        let old_entry = plan
            .entries
            .iter()
            .find(|entry| entry.path == old_str)
            .expect("the session should restore the rename source");
        let new_entry = plan
            .entries
            .iter()
            .find(|entry| entry.path == new_str)
            .expect("the session should remove the rename destination");
        match &old_entry.action {
            RestoreAction::Write { source } => assert_eq!(source.hash, "source_hash"),
            other => panic!("expected source write, got {other:?}"),
        }
        assert!(matches!(
            new_entry.action,
            RestoreAction::DeleteCreatedAfterTarget
        ));
    }

    /// Retention may remove the pre-session event while preserving its snapshot
    /// through the first in-session modification's `previous_hash`. Recovery
    /// must write that baseline instead of treating the existing file as new.
    #[test]
    fn plan_session_restore_uses_previous_hash_after_baseline_pruned() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("src/main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "changed during session").unwrap();
        let file_str = file.to_string_lossy().to_string();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        db.start_session(project.id, "modify-run", "manual")
            .unwrap();
        // This is the post-retention state: the older baseline row is gone,
        // while the surviving modification still pins its snapshot.
        db.insert_event(
            project.id,
            &file_str,
            "MODIFIED",
            Some("during_hash"),
            Some("baseline_hash"),
            None,
            None,
            Some(22),
        )
        .unwrap();
        let session = db.stop_active_session(project.id).unwrap().unwrap();

        assert!(
            db.get_event_at_session_start(&session, &file_str)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_live_hashes(project.id)
                .unwrap()
                .contains("baseline_hash")
        );

        let plan = plan_paths_restore_at_session_start(
            &db,
            &project,
            &root,
            std::slice::from_ref(&file_str),
            &session,
        )
        .unwrap();

        assert_eq!(plan.entries.len(), 1);
        match &plan.entries[0].action {
            RestoreAction::Write { source } => assert_eq!(source.hash, "baseline_hash"),
            other => panic!("expected baseline write, got {other:?}"),
        }
    }

    /// Group recovery should only plan changes for the selected group's paths.
    #[test]
    fn plan_paths_restore_leaves_unselected_paths_alone() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let auth = root.join("src/auth/login.rs");
        let billing = root.join("src/billing/invoice.rs");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::create_dir_all(billing.parent().unwrap()).unwrap();
        std::fs::write(&auth, "broken auth").unwrap();
        std::fs::write(&billing, "good billing").unwrap();

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        let auth_str = auth.to_string_lossy().to_string();
        let billing_str = billing.to_string_lossy().to_string();
        let session = db.start_session(project.id, "agent-run", "manual").unwrap();
        db.insert_event(
            project.id,
            &auth_str,
            "MODIFIED",
            Some("auth_after"),
            Some("auth_before"),
            None,
            None,
            Some(11),
        )
        .unwrap();
        db.insert_event(
            project.id,
            &billing_str,
            "MODIFIED",
            Some("billing_after"),
            Some("billing_before"),
            None,
            None,
            Some(12),
        )
        .unwrap();

        let plan = plan_paths_restore_at_session_start(
            &db,
            &project,
            &root,
            std::slice::from_ref(&auth_str),
            &session,
        )
        .unwrap();

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].rel_path, "src/auth/login.rs");
        assert!(!plan.entries.iter().any(|entry| entry.path == billing_str));
    }
}
