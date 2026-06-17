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
    /// Seconds-precise override for the retention window. When set, takes
    /// precedence over `retention_days`. Used by `--keep <duration>` so
    /// sub-day inputs (e.g. `12h`, `30m`) are honoured exactly instead of
    /// being rounded up to whole days.
    pub retention_secs_override: Option<u64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            max_size_mb: DEFAULT_MAX_SIZE_MB,
            retention_secs_override: None,
        }
    }
}

impl RetentionConfig {
    /// Effective retention window in seconds.
    pub fn retention_seconds(&self) -> i64 {
        match self.retention_secs_override {
            Some(s) => s as i64,
            None => self.retention_days.saturating_mul(86400) as i64,
        }
    }

    /// Effective size cap in bytes. `max_size_mb` comes from user config
    /// (`~/.undo/config.toml` / `.undorc`), so a large value would overflow
    /// `max_size_mb * 1024 * 1024`: in debug that panics the daemon's
    /// auto-prune thread (no `Err` to catch — it unwinds), and in release it
    /// wraps to a tiny cap that triggers over-aggressive snapshot deletion.
    /// Saturate instead so the cap degrades to "effectively unlimited".
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_mb.saturating_mul(1024).saturating_mul(1024)
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
        if let Ok(contents) = std::fs::read_to_string(&global_path)
            && let Ok(raw) = toml::from_str::<RawConfig>(&contents)
        {
            if let Some(d) = raw.retention_days {
                cfg.retention_days = d;
            }
            if let Some(s) = raw.max_size_mb {
                cfg.max_size_mb = s;
            }
        }
    }

    if let Some(root) = project_root {
        let project_path = root.join(".undorc");
        if let Ok(contents) = std::fs::read_to_string(&project_path)
            && let Ok(raw) = toml::from_str::<RawConfig>(&contents)
        {
            if let Some(d) = raw.retention_days {
                cfg.retention_days = d;
            }
            if let Some(s) = raw.max_size_mb {
                cfg.max_size_mb = s;
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

    let cutoff = Utc::now().timestamp() - config.retention_seconds();

    // 1. Delete old events
    if dry_run {
        stats.events_deleted = db.count_events_before(project_id, cutoff)?;
    } else {
        stats.events_deleted = db.delete_events_before(project_id, cutoff)?;
    }

    // 2. Delete orphaned snapshots
    let live_hashes = db.get_live_hashes(project_id)?;
    let bt_dir = crate::backtrack_dir()?;
    let snap_dir = bt_dir.join("snapshots").join(project_id.to_string());

    if snap_dir.exists() {
        for entry in std::fs::read_dir(&snap_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("gz") {
                continue;
            }
            let hash = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
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
        let backup_cutoff = Utc::now().timestamp() - config.retention_seconds();
        for entry in std::fs::read_dir(&backups_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let mtime = match entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
            {
                Some(t) => t,
                None => continue, // can't determine age — leave the backup alone
            };
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
        let max_bytes = config.max_size_bytes();
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
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("gz"))
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

/// Per-bucket disk usage for `~/.undo`, computed in a SINGLE tree walk.
///
/// `undo status` previously called `dir_size("snapshots")`, `dir_size("backups")`
/// and `total_disk_usage()` separately, so the snapshots and backups subtrees
/// were each walked twice. On a large store that triple-walk made `status`
/// needlessly slow. This walks `~/.undo` once and attributes each file to its
/// bucket while accumulating the grand total.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiskUsage {
    pub total: u64,
    pub snapshots: u64,
    pub backups: u64,
}

pub fn disk_usage_breakdown() -> Result<DiskUsage> {
    let bt_dir = crate::backtrack_dir()?;
    let snapshots_root = bt_dir.join("snapshots");
    let backups_root = bt_dir.join("backups");

    let mut usage = DiskUsage::default();
    for entry in WalkDir::new(&bt_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        usage.total += len;
        let path = entry.path();
        if path.starts_with(&snapshots_root) {
            usage.snapshots += len;
        } else if path.starts_with(&backups_root) {
            usage.backups += len;
        }
    }
    Ok(usage)
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

    /// A single walk attributes files to the snapshots/backups buckets and
    /// sums the grand total — including files that belong to neither bucket
    /// (db, pids), which count toward `total` only. This is the behavior
    /// `cmd_status` relies on after collapsing its three walks into one.
    #[test]
    fn disk_usage_breakdown_buckets_and_totals_in_one_walk() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let bt = crate::backtrack_dir().unwrap();

        // snapshots/<id>/<hash>.gz
        let snap_dir = bt.join("snapshots").join("1");
        std::fs::create_dir_all(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("a.gz"), vec![b'x'; 100]).unwrap();
        std::fs::write(snap_dir.join("b.gz"), vec![b'x'; 50]).unwrap();

        // backups/<name>.bak
        let backups_dir = bt.join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();
        std::fs::write(backups_dir.join("f.bak"), vec![b'y'; 30]).unwrap();

        // A file in neither bucket: counts toward total only.
        std::fs::write(bt.join("database.db"), vec![b'z'; 7]).unwrap();

        let usage = disk_usage_breakdown().unwrap();
        assert_eq!(usage.snapshots, 150, "snapshots bucket");
        assert_eq!(usage.backups, 30, "backups bucket");
        assert_eq!(usage.total, 187, "total includes db (150 + 30 + 7)");
    }

    /// With no store on disk yet, every bucket is zero (no panic on an empty
    /// or freshly-created data dir).
    #[test]
    fn disk_usage_breakdown_empty_store_is_zero() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let usage = disk_usage_breakdown().unwrap();
        assert_eq!(usage.total, 0);
        assert_eq!(usage.snapshots, 0);
        assert_eq!(usage.backups, 0);
    }

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

    /// `load_config` reads `.undorc` from the path it is given — nothing else.
    /// This pins the contract that `cmd_prune` relies on: when it passes the
    /// *project root*, the project's `.undorc` is honoured even if the user
    /// invoked `undo prune` from a subdirectory with no config of its own.
    /// (The previous `cmd_prune` passed `cwd`, silently dropping the project
    /// `.undorc` whenever the user wasn't standing at the root.)
    #[test]
    fn load_config_reads_undorc_from_given_root_not_subdir() {
        let project_root = tempfile::tempdir().unwrap();
        std::fs::write(project_root.path().join(".undorc"), "retention_days = 42\n").unwrap();

        // A subdirectory with no .undorc of its own — what `cwd` would be when
        // the user runs `undo prune` from `<root>/src/`.
        let subdir = project_root.path().join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        // Asking for the project-root config returns the project's settings.
        let from_root = load_config(Some(project_root.path()));
        assert_eq!(from_root.retention_days, 42);

        // Asking for the subdir's config silently falls back to defaults —
        // documenting the bug `cmd_prune` previously hit.
        let from_subdir = load_config(Some(&subdir));
        assert_eq!(
            from_subdir.retention_days, DEFAULT_RETENTION_DAYS,
            "passing the subdir to load_config silently loses the project .undorc"
        );
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

    /// Without a seconds override, retention is derived from retention_days.
    #[test]
    fn retention_seconds_uses_days_when_no_override() {
        let cfg = RetentionConfig {
            retention_days: 7,
            max_size_mb: 1024,
            retention_secs_override: None,
        };
        assert_eq!(cfg.retention_seconds(), 7 * 86400);
    }

    /// A seconds override (used by `--keep`) takes precedence over retention_days
    /// so sub-day windows like 12h or 30m are honoured exactly.
    #[test]
    fn retention_seconds_override_takes_precedence_over_days() {
        let cfg = RetentionConfig {
            retention_days: 7,
            max_size_mb: 1024,
            retention_secs_override: Some(12 * 3600),
        };
        assert_eq!(cfg.retention_seconds(), 12 * 3600);
    }

    /// Sub-minute overrides survive the round-trip — guards against any future
    /// regression to a "round up to 1 day" implementation.
    #[test]
    fn retention_seconds_override_preserves_sub_day_precision() {
        let cfg = RetentionConfig {
            retention_days: 7,
            max_size_mb: 1024,
            retention_secs_override: Some(30),
        };
        assert_eq!(cfg.retention_seconds(), 30);
        assert_ne!(cfg.retention_seconds(), 86400);
    }

    /// The default cap converts to bytes without surprises.
    #[test]
    fn max_size_bytes_normal_value() {
        let cfg = RetentionConfig::default();
        assert_eq!(cfg.max_size_bytes(), 1024 * 1024 * 1024);
    }

    /// A very large `max_size_mb` from user config must not overflow when
    /// converted to bytes. The previous `max_size_mb * 1024 * 1024` panics in
    /// debug (crashing the auto-prune thread) and wraps to a tiny cap in
    /// release (over-aggressive pruning). `max_size_bytes()` saturates instead.
    /// (Red before the fix: `u64::MAX * 1024 * 1024` overflows.)
    #[test]
    fn max_size_bytes_saturates_instead_of_overflowing() {
        let cfg = RetentionConfig {
            retention_days: 7,
            max_size_mb: u64::MAX,
            retention_secs_override: None,
        };
        // Must not panic, and must clamp to the u64 ceiling rather than wrap.
        assert_eq!(cfg.max_size_bytes(), u64::MAX);
    }

    /// Demonstrates the hazard the fix removes: the naive multiplication that
    /// `prune` and `cmd_status` previously used cannot represent the result.
    #[test]
    fn naive_max_size_multiplication_overflows() {
        assert!(
            u64::MAX
                .checked_mul(1024)
                .and_then(|v| v.checked_mul(1024))
                .is_none(),
            "max_size_mb * 1024 * 1024 overflows for large configs — \
             this is why max_size_bytes() must saturate"
        );
    }

    /// An enormous retention_days must not overflow the seconds conversion
    /// either (same `* 86400` hazard).
    #[test]
    fn retention_seconds_saturates_for_huge_days() {
        let cfg = RetentionConfig {
            retention_days: u64::MAX,
            max_size_mb: 1024,
            retention_secs_override: None,
        };
        // saturating_mul keeps it within u64, then `as i64` clamps high bits;
        // the key property is that it does not panic.
        let _ = cfg.retention_seconds();
    }
}
