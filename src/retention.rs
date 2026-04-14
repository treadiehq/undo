use anyhow::Result;
use chrono::Utc;
use serde::Deserialize;
use std::path::Path;
use walkdir::WalkDir;

use crate::db::Database;

const DEFAULT_RETENTION_DAYS: u64 = 7;
const DEFAULT_MAX_SIZE_MB: u64 = 1024;

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    retention_days: Option<u64>,
    max_size_mb: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub retention_days: u64,
    pub max_size_mb: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            max_size_mb: DEFAULT_MAX_SIZE_MB,
        }
    }
}

pub struct PruneStats {
    pub events_deleted: u64,
    pub snapshots_deleted: u64,
    pub backups_deleted: u64,
    pub bytes_freed: u64,
}

/// Load config: hardcoded defaults -> ~/.undo/config.toml -> .undorc in project root.
/// Each layer overrides the previous.
pub fn load_config(project_root: Option<&Path>) -> RetentionConfig {
    let mut cfg = RetentionConfig::default();

    if let Ok(bt_dir) = crate::backtrack_dir() {
        let global_path = bt_dir.join("config.toml");
        if let Ok(contents) = std::fs::read_to_string(&global_path) {
            if let Ok(raw) = toml::from_str::<RawConfig>(&contents) {
                if let Some(d) = raw.retention_days {
                    cfg.retention_days = d;
                }
                if let Some(s) = raw.max_size_mb {
                    cfg.max_size_mb = s;
                }
            }
        }
    }

    if let Some(root) = project_root {
        let project_path = root.join(".undorc");
        if let Ok(contents) = std::fs::read_to_string(&project_path) {
            if let Ok(raw) = toml::from_str::<RawConfig>(&contents) {
                if let Some(d) = raw.retention_days {
                    cfg.retention_days = d;
                }
                if let Some(s) = raw.max_size_mb {
                    cfg.max_size_mb = s;
                }
            }
        }
    }

    cfg
}

/// Run the full prune cycle for one project.
pub fn prune(
    db: &Database,
    project_id: i64,
    config: &RetentionConfig,
    dry_run: bool,
) -> Result<PruneStats> {
    let mut stats = PruneStats {
        events_deleted: 0,
        snapshots_deleted: 0,
        backups_deleted: 0,
        bytes_freed: 0,
    };

    let cutoff = Utc::now().timestamp() - (config.retention_days as i64 * 86400);

    // 1. Delete old events
    if dry_run {
        stats.events_deleted = db.count_events_before(project_id, cutoff)?;
    } else {
        stats.events_deleted = db.delete_events_before(project_id, cutoff)?;
    }

    // 2. Delete orphaned snapshots
    let live_hashes = db.get_live_hashes(project_id)?;
    let bt_dir = crate::backtrack_dir()?;
    let snap_dir = bt_dir
        .join("snapshots")
        .join(project_id.to_string());

    if snap_dir.exists() {
        for entry in std::fs::read_dir(&snap_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("gz") {
                continue;
            }
            let hash = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !live_hashes.contains(hash) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if !dry_run {
                    let _ = std::fs::remove_file(&path);
                }
                stats.snapshots_deleted += 1;
                stats.bytes_freed += size;
            }
        }
    }

    // 3. Delete old backups
    let backups_dir = bt_dir.join("backups");
    if backups_dir.exists() {
        let backup_cutoff = Utc::now().timestamp() - (config.retention_days as i64 * 86400);
        for entry in std::fs::read_dir(&backups_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if mtime < backup_cutoff {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if !dry_run {
                    let _ = std::fs::remove_file(&path);
                }
                stats.backups_deleted += 1;
                stats.bytes_freed += size;
            }
        }
    }

    // 4. Size backstop: if still over max_size_mb, prune oldest unreferenced snapshots
    if !dry_run {
        let max_bytes = config.max_size_mb * 1024 * 1024;
        let mut current = total_disk_usage()?;
        if current > max_bytes {
            let all_projects = db.get_all_project_ids()?;
            'outer: for pid in &all_projects {
                let pid_live_hashes = db.get_live_hashes(*pid)?;
                let sdir = bt_dir.join("snapshots").join(pid.to_string());
                if !sdir.exists() {
                    continue;
                }
                let mut files: Vec<_> = std::fs::read_dir(&sdir)?
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path().extension().and_then(|x| x.to_str()) == Some("gz")
                    })
                    .collect();
                files.sort_by_key(|e| {
                    e.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                });
                for entry in files {
                    if current <= max_bytes {
                        break 'outer;
                    }
                    let hash = entry
                        .path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    if pid_live_hashes.contains(hash.as_str()) {
                        continue;
                    }
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    let _ = std::fs::remove_file(entry.path());
                    current = current.saturating_sub(size);
                    stats.snapshots_deleted += 1;
                    stats.bytes_freed += size;
                }
            }
            if current > max_bytes {
                eprintln!(
                    "{}warning:{} disk usage ({}) still exceeds cap ({}) — \
                     remaining snapshots are referenced by live events. \
                     Consider increasing max_size_mb in .undorc or ~/.undo/config.toml.",
                    crate::YELLOW,
                    crate::RESET,
                    format_size(current),
                    format_size(max_bytes),
                );
            }
        }
    }

    Ok(stats)
}

/// Total size of everything under ~/.undo/ in bytes.
pub fn total_disk_usage() -> Result<u64> {
    let bt_dir = crate::backtrack_dir()?;
    let mut total: u64 = 0;
    for entry in WalkDir::new(&bt_dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

/// Size of a specific subdirectory under ~/.undo/ in bytes.
pub fn dir_size(subdir: &str) -> Result<u64> {
    let bt_dir = crate::backtrack_dir()?;
    let target = bt_dir.join(subdir);
    if !target.exists() {
        return Ok(0);
    }
    let mut total: u64 = 0;
    for entry in WalkDir::new(&target).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in defaults are 7 days retention and a 1 GiB size cap.
    #[test]
    fn default_config_values() {
        let cfg = RetentionConfig::default();
        assert_eq!(cfg.retention_days, 7);
        assert_eq!(cfg.max_size_mb, 1024);
    }

    /// When no config files are present, load_config returns the built-in defaults.
    #[test]
    fn load_config_without_files_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_config(Some(dir.path()));
        assert_eq!(cfg.retention_days, 7);
        assert_eq!(cfg.max_size_mb, 1024);
    }

    /// A .undorc in the project root overrides only the fields it specifies; others keep defaults.
    #[test]
    fn undorc_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".undorc"), "retention_days = 30\n").unwrap();
        let cfg = load_config(Some(dir.path()));
        assert_eq!(cfg.retention_days, 30);
        assert_eq!(cfg.max_size_mb, 1024);
    }

    /// Values under 1 KiB are formatted with a B suffix.
    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(500), "500 B");
    }

    /// Values in the KiB range are formatted with a KB suffix.
    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(2048), "2.0 KB");
    }

    /// Values in the MiB range are formatted with an MB suffix.
    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    /// Values in the GiB range are formatted with a GB suffix.
    #[test]
    fn format_size_gigabytes() {
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    /// Both retention_days and max_size_mb can be overridden together in a single .undorc.
    #[test]
    fn undorc_overrides_both_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".undorc"),
            "retention_days = 14\nmax_size_mb = 512\n",
        )
        .unwrap();
        let cfg = load_config(Some(dir.path()));
        assert_eq!(cfg.retention_days, 14);
        assert_eq!(cfg.max_size_mb, 512);
    }
}
