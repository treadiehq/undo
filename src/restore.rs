use crate::db::Database;
use crate::duration;
use crate::snapshots;
use crate::{find_project, GREEN, RESET};
use anyhow::Result;
use chrono::Utc;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn cmd_restore(path_str: &str, duration_str: &str) -> Result<()> {
    let secs = duration::parse_duration(duration_str)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;

    let abs_path = crate::safe_resolve_path(&cwd, path_str, &project.root_path)?;
    let abs_path_str = abs_path.to_string_lossy().to_string();

    // Refuse to write through symlinks — prevent overwriting files outside the project.
    if abs_path.exists() && abs_path.symlink_metadata()?.file_type().is_symlink() {
        anyhow::bail!("refusing to restore through symlink '{}'", path_str);
    }

    let target_time = Utc::now().timestamp() - secs;

    let event = match db.get_event_at_time(project.id, &abs_path_str, target_time)? {
        Some(e) => e,
        None => match db.get_oldest_event(project.id, &abs_path_str)? {
            Some(e) => {
                let age = Utc::now().timestamp() - e.timestamp;
                println!(
                    "No snapshot from {} ago — falling back to earliest available (from {}).",
                    duration_str,
                    duration::format_elapsed(age)
                );
                e
            }
            None => {
                println!("No snapshots found for this file.");
                return Ok(());
            }
        },
    };

    let hash = match &event.current_hash {
        Some(h) => h,
        None => {
            println!("No restorable snapshot found at that time.");
            return Ok(());
        }
    };

    let content = snapshots::load(project.id, hash)?;

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

    let elapsed = Utc::now().timestamp() - event.timestamp;
    let ago = duration::format_elapsed(elapsed);
    let rel = crate::relative_path(&abs_path_str, &project.root_path);

    println!(
        "{}Restored{} {} from snapshot captured {}.",
        GREEN, RESET, rel, ago
    );

    Ok(())
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

/// Same pattern as `snapshots::save`: `create_new` temp, full write, `rename` into place.
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
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    write_result.map_err(|e| e.into())
}
