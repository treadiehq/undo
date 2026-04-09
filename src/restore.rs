use anyhow::Result;
use chrono::Utc;
use std::path::PathBuf;

use crate::db::Database;
use crate::duration;
use crate::snapshots;
use crate::{find_project, GREEN, RESET};

pub fn cmd_restore(path_str: &str, duration_str: &str) -> Result<()> {
    let secs = duration::parse_duration(duration_str)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;

    let abs_path = cwd.join(path_str);
    let abs_path = if abs_path.exists() {
        abs_path.canonicalize()?
    } else {
        abs_path
    };
    let abs_path_str = abs_path.to_string_lossy().to_string();

    let target_time = Utc::now().timestamp() - secs;

    let event = match db.get_event_at_time(project.id, &abs_path_str, target_time)? {
        Some(e) => e,
        None => {
            println!("No snapshot found for this file at that time.");
            return Ok(());
        }
    };

    let hash = match &event.current_hash {
        Some(h) => h,
        None => {
            println!("No restorable snapshot found at that time.");
            return Ok(());
        }
    };

    let content = snapshots::load(project.id, hash)?;

    // Safety backup before overwriting
    if abs_path.exists() {
        let filename = abs_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let ts = Utc::now().timestamp();
        let backup_path = PathBuf::from(format!(
            "/tmp/undo-restore-{}-{}.bak",
            filename, ts
        ));
        std::fs::copy(&abs_path, &backup_path)?;
        println!("Backup of current file saved to {}", backup_path.display());
    }

    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&abs_path, &content)?;

    let elapsed = Utc::now().timestamp() - event.timestamp;
    let ago = duration::format_elapsed(elapsed);
    let rel = crate::relative_path(&abs_path_str, &project.root_path);

    println!(
        "{}Restored{} {} from snapshot captured {}.",
        GREEN, RESET, rel, ago
    );

    Ok(())
}
