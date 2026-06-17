use crate::db::Database;
use crate::duration;
use crate::snapshots;
use crate::{GREEN, RESET, find_project};
use anyhow::Result;
use chrono::Utc;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn cmd_restore(path_str: &str, duration_str: &str) -> Result<()> {
    let secs = duration::parse_duration(duration_str)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;

    // Refuse to write through symlinks — prevent overwriting files outside the project.
    // This MUST be checked on the unresolved path: `safe_resolve_path` calls
    // `canonicalize()`, which follows symlinks, so checking the resolved path
    // would never see a symlink and the guard would be dead code.
    let raw_path = cwd.join(path_str);
    if let Ok(meta) = raw_path.symlink_metadata()
        && meta.file_type().is_symlink()
    {
        anyhow::bail!("refusing to restore through symlink '{}'", path_str);
    }

    let abs_path = crate::safe_resolve_path(&cwd, path_str, &project.root_path)?;
    let abs_path_str = abs_path.to_string_lossy().to_string();

    // saturating_sub: `secs` can be up to i64::MAX (parse_duration accepts it),
    // so a bare `now - secs` underflows — a debug-build panic, and in release a
    // wrap to a large positive time. Saturate to i64::MIN instead.
    let target_time = Utc::now().timestamp().saturating_sub(secs);

    let source = match resolve_restore_source(&db, project.id, &abs_path_str, target_time)? {
        Some(s) => s,
        None => {
            println!("No snapshots found for this file.");
            return Ok(());
        }
    };

    match source.kind {
        RestoreKind::Exact => {}
        RestoreKind::OldestFallback => {
            let age = Utc::now().timestamp() - source.timestamp;
            println!(
                "No snapshot from {} ago — falling back to earliest available (from {}).",
                duration_str,
                duration::format_elapsed(age)
            );
        }
        RestoreKind::DeletedFallback => {
            let age = Utc::now().timestamp() - source.timestamp;
            println!(
                "File was deleted {} ago — restoring its last recorded contents.",
                duration::format_elapsed(age)
            );
        }
    }

    let content = snapshots::load(project.id, &source.hash)?;

    // Safety backup before overwriting.
    // Stored in ~/.undo/backups/ rather than /tmp so it survives a reboot —
    // /tmp is cleared on restart, which would defeat the purpose of the backup.
    if abs_path.exists() {
        use std::os::unix::fs::PermissionsExt;
        let filename = abs_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let ts = Utc::now().timestamp();
        let backups_dir = crate::backtrack_dir()?.join("backups");
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(&backups_dir)?;
        // Restrict backups dir to owner-only
        let _ = std::fs::set_permissions(&backups_dir, std::fs::Permissions::from_mode(0o700));
        let backup_path = backups_dir.join(format!("{}_{}.bak", filename, ts));
        std::fs::copy(&abs_path, &backup_path)?;
        // Restrict backup file to owner-only
        let _ = std::fs::set_permissions(&backup_path, std::fs::Permissions::from_mode(0o600));
        println!("Backup of current file saved to {}", backup_path.display());
    }

    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write to a sibling temp file then rename atomically so an interrupted
    // restore never leaves a partially-written target. The temp name must never
    // collide with the target path: `with_extension("undo_tmp")` is wrong for
    // paths that already end in `.undo_tmp` (it would write the destination in place).
    write_restore_atomically(&abs_path, &content)?;

    let elapsed = Utc::now().timestamp() - source.timestamp;
    let ago = duration::format_elapsed(elapsed);
    let rel = crate::relative_path(&abs_path_str, &project.root_path);

    println!(
        "{}Restored{} {} from snapshot captured {}.",
        GREEN, RESET, rel, ago
    );

    Ok(())
}

/// Which snapshot content `restore` should write, and where it came from.
#[derive(Debug)]
struct RestoreSource {
    hash: String,
    timestamp: i64,
    kind: RestoreKind,
}

#[derive(Debug, PartialEq)]
enum RestoreKind {
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
/// 2. the earliest recorded snapshot (when the window predates all history),
/// 3. the last contents of a deleted file, recovered from the most recent
///    DELETED event's `previous_hash`.
///
/// Step 3 is the fix for files that were deleted after their creating event
/// aged out of retention: previously both earlier lookups returned `None` and
/// restore reported "No snapshots found", even though the deletion itself was
/// well within the retention window and the snapshot was still on disk.
fn resolve_restore_source(
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

    use super::{RestoreKind, resolve_restore_source};
    use crate::db::Database;

    fn mem_db() -> (Database, i64) {
        let db = Database::open_in_memory().unwrap();
        let p = db
            .get_or_create_project(std::path::Path::new("/proj"))
            .unwrap();
        (db, p.id)
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
}
