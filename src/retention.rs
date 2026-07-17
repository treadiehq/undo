use anyhow::Result;
use chrono::Utc;
use serde::Deserialize;
use std::path::Path;
use walkdir::WalkDir;

use crate::db::Database;

const DEFAULT_RETENTION_DAYS: u64 = 7;
const DEFAULT_MAX_SIZE_MB: u64 = 1024;

/// Minimum age before a leftover snapshot temp file (`<hash>.gz.tmp.<pid>.<seq>`)
/// is treated as leaked and reclaimable. A durable snapshot write creates the
/// temp, fsyncs, and renames it into place within milliseconds, removing it on
/// error — so any temp older than this can only be the residue of a hard kill or
/// power loss between create and rename. The generous margin guarantees an
/// in-flight write from a concurrent `undo` process is never mistaken for leaked.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

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
    ///
    /// Clamped to `i64::MAX`. `retention_days` comes from user config, so a huge
    /// value would make `retention_days * 86400` exceed `i64::MAX`; the bare
    /// `as i64` cast then *wraps to a negative number*, and `now - retention_seconds()`
    /// becomes a cutoff in the FUTURE — which makes prune delete the entire
    /// history (every timestamp is "before" a future cutoff). Saturating keeps
    /// the window non-negative; callers pair it with `saturating_sub` so an
    /// effectively-infinite window degrades to "keep everything", never "delete
    /// everything".
    pub fn retention_seconds(&self) -> i64 {
        let ceiling = i64::MAX as u64;
        match self.retention_secs_override {
            Some(s) => s.min(ceiling) as i64,
            None => self.retention_days.saturating_mul(86400).min(ceiling) as i64,
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

/// True if `path` names an unpublished snapshot temp file, i.e. the
/// `<hash>.gz.tmp.<pid>.<seq>` form written by `snapshots::write_snapshot_in`
/// before the atomic rename. Real snapshots are `<hash>.gz`; hashes are hex, so
/// they never contain `.gz.tmp.`, making the substring an unambiguous marker.
fn is_snapshot_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(".gz.tmp."))
}

fn reap_stale_snapshot_temps(snapshots_root: &Path, dry_run: bool) -> Result<u64> {
    if !snapshots_root.exists() {
        return Ok(0);
    }

    // Anything created before this instant minus the staleness margin is old
    // enough to be a leaked temp rather than an in-flight write. `checked_sub`
    // guards the (pathological) case of a system clock near the epoch.
    let stale_temp_cutoff = std::time::SystemTime::now().checked_sub(STALE_TEMP_AGE);
    let mut bytes_freed = 0;

    for project_dir in std::fs::read_dir(snapshots_root)? {
        let project_dir = project_dir?;
        if !project_dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }

        for entry in std::fs::read_dir(project_dir.path())? {
            let entry = entry?;
            let path = entry.path();
            // Reap leaked snapshot temp files (`<hash>.gz.tmp.<pid>.<seq>`) from
            // writes interrupted by a hard kill or power loss. The global size
            // backstop measures the whole store, so leaked temps from any project
            // must be reclaimed before deciding whether snapshots need pruning.
            if !is_snapshot_temp(&path) {
                continue;
            }

            if let Some(meta) = entry.metadata().ok().filter(|m| {
                stale_temp_cutoff
                    .is_some_and(|cutoff| m.modified().is_ok_and(|mtime| mtime < cutoff))
            }) {
                if !dry_run {
                    let _ = std::fs::remove_file(&path);
                }
                bytes_freed += meta.len();
            }
        }
    }

    Ok(bytes_freed)
}

/// Run the full prune cycle for one project.
pub fn prune(
    db: &Database,
    project_id: i64,
    config: &RetentionConfig,
    dry_run: bool,
) -> Result<PruneStats> {
    // Snapshot publishers hold the shared side of this lock until their
    // database references commit. Taking it exclusively before deleting old
    // events closes both race directions: an in-flight publisher finishes
    // first, while a new publisher waits before creating a visible snapshot.
    let _gc_guard = crate::snapshots::acquire_gc_guard()?;

    let mut stats = PruneStats {
        events_deleted: 0,
        snapshots_deleted: 0,
        backups_deleted: 0,
        bytes_freed: 0,
    };

    // saturating_sub so an effectively-infinite retention window (a huge
    // `retention_days`) yields a cutoff far in the PAST ("keep everything"),
    // never an underflow that wraps into the future and deletes everything.
    let cutoff = Utc::now()
        .timestamp()
        .saturating_sub(config.retention_seconds());

    // 1. Delete old events
    if dry_run {
        stats.events_deleted = db.count_events_before(project_id, cutoff)?;
    } else {
        stats.events_deleted = db.delete_events_before(project_id, cutoff)?;
    }

    // 2. Delete orphaned snapshots
    let live_hashes = db.get_live_hashes(project_id)?;
    let bt_dir = crate::backtrack_dir()?;
    let snapshots_root = bt_dir.join("snapshots");
    stats.bytes_freed += reap_stale_snapshot_temps(&snapshots_root, dry_run)?;
    let snap_dir = snapshots_root.join(project_id.to_string());

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
        let backup_cutoff = Utc::now()
            .timestamp()
            .saturating_sub(config.retention_seconds());
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
                    "{}warning:{} Saved versions still needed: storage is {} (limit {}). \
                     Undo kept the remaining versions because recorded file changes still reference them. \
                     Increase max_size_mb in .undorc or ~/.undo/config.toml to keep them without exceeding the limit.",
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

    /// An enormous retention_days must not overflow the seconds conversion.
    /// The bare `saturating_mul(86400) as i64` *wraps to a negative value* for
    /// large day counts; `retention_seconds()` must instead clamp to a
    /// non-negative window (`i64::MAX`) so the downstream `now - window` cutoff
    /// can never land in the future. (Red before the `.min(i64::MAX as u64)`
    /// clamp: `u64::MAX` days returns `-1`.)
    #[test]
    fn retention_seconds_saturates_for_huge_days() {
        for days in [u64::MAX, 1_000_000_000_000_000, 213_503_982_334_601] {
            let cfg = RetentionConfig {
                retention_days: days,
                max_size_mb: 1024,
                retention_secs_override: None,
            };
            let secs = cfg.retention_seconds();
            assert!(
                secs >= 0,
                "retention window must never be negative (got {secs} for {days} days) — \
                 a negative window makes prune's cutoff land in the future"
            );
            assert_eq!(secs, i64::MAX, "an oversized window must clamp to i64::MAX");
        }
    }

    /// End-to-end guard for the data-loss footgun: with an enormous
    /// `retention_days`, `prune` must KEEP recent events, not wipe them. Before
    /// the fix, `retention_seconds()` wrapped negative and the cutoff landed in
    /// the future, so `delete_events_before` removed every event on the next
    /// (hourly) auto-prune.
    #[test]
    fn prune_with_huge_retention_days_keeps_recent_events() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(Path::new("/proj")).unwrap();
        // A brand-new event (timestamped "now") that must survive pruning.
        db.insert_event(
            project.id,
            "/proj/a.rs",
            "CREATED",
            Some("h"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(db.count_events(project.id).unwrap(), 1);

        let cfg = RetentionConfig {
            retention_days: 1_000_000_000_000_000, // wraps negative without the clamp
            max_size_mb: 1024,
            retention_secs_override: None,
        };
        prune(&db, project.id, &cfg, false).unwrap();

        assert_eq!(
            db.count_events(project.id).unwrap(),
            1,
            "a huge retention window must keep everything, not delete it"
        );
    }

    /// `is_snapshot_temp` recognises the `<hash>.gz.tmp.<pid>.<seq>` form and
    /// nothing else — a published `<hash>.gz` snapshot must never match.
    #[test]
    fn is_snapshot_temp_matches_only_temp_writes() {
        assert!(is_snapshot_temp(Path::new("/s/1/deadbeef.gz.tmp.123.4")));
        assert!(!is_snapshot_temp(Path::new("/s/1/deadbeef.gz")));
        assert!(!is_snapshot_temp(Path::new("/s/1/deadbeef")));
    }

    /// A leaked snapshot temp file (from a write killed between create and rename)
    /// must be reaped by prune once it ages past `STALE_TEMP_AGE`, while a fresh
    /// in-flight temp is preserved. Before the fix nothing ever reclaimed these —
    /// the `.gz`-only extension filter skipped them — so they accumulated across
    /// crashes and inflated disk usage indefinitely.
    /// (Red before the reaping branch: the stale temp survives prune.)
    #[test]
    fn prune_reaps_stale_snapshot_temp_files() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(Path::new("/proj")).unwrap();

        let snap_dir = crate::backtrack_dir()
            .unwrap()
            .join("snapshots")
            .join(project.id.to_string());
        std::fs::create_dir_all(&snap_dir).unwrap();

        // A leaked temp from an interrupted durable write, backdated well past the
        // staleness threshold.
        let stale = snap_dir.join("deadbeef.gz.tmp.999.1");
        std::fs::write(&stale, b"half-written gzip").unwrap();
        set_mtime_secs_ago(&stale, STALE_TEMP_AGE.as_secs() * 2);

        // A fresh temp from a write happening right now must be left untouched.
        let fresh = snap_dir.join("cafef00d.gz.tmp.1000.2");
        std::fs::write(&fresh, b"in flight").unwrap();

        let stats = prune(&db, project.id, &RetentionConfig::default(), false).unwrap();

        assert!(!stale.exists(), "a stale leaked temp file must be reaped");
        assert!(
            fresh.exists(),
            "a fresh in-flight temp file must be preserved"
        );
        assert!(
            stats.bytes_freed >= b"half-written gzip".len() as u64,
            "reaped temp bytes must be reported as freed"
        );
    }

    /// A dry run must report-but-not-delete a stale temp, mirroring how the rest of
    /// prune treats `--dry-run`.
    #[test]
    fn prune_dry_run_keeps_stale_temp_files() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(Path::new("/proj")).unwrap();

        let snap_dir = crate::backtrack_dir()
            .unwrap()
            .join("snapshots")
            .join(project.id.to_string());
        std::fs::create_dir_all(&snap_dir).unwrap();

        let stale = snap_dir.join("deadbeef.gz.tmp.999.1");
        std::fs::write(&stale, b"half-written gzip").unwrap();
        set_mtime_secs_ago(&stale, STALE_TEMP_AGE.as_secs() * 2);

        prune(&db, project.id, &RetentionConfig::default(), true).unwrap();

        assert!(stale.exists(), "dry run must not delete the stale temp");
    }

    /// The size backstop is global, so stale temps in another project's snapshot
    /// dir must be reclaimed before deciding whether the store still exceeds
    /// `max_size_mb`. Otherwise a leaked temp outside the current project can
    /// keep `~/.undo` over the cap forever.
    #[test]
    fn prune_reaps_stale_snapshot_temp_files_across_projects_before_size_backstop() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let db = Database::open_in_memory().unwrap();
        let current_project = db.get_or_create_project(Path::new("/current")).unwrap();
        let other_project = db.get_or_create_project(Path::new("/other")).unwrap();

        let other_snap_dir = crate::backtrack_dir()
            .unwrap()
            .join("snapshots")
            .join(other_project.id.to_string());
        std::fs::create_dir_all(&other_snap_dir).unwrap();

        let stale = other_snap_dir.join("deadbeef.gz.tmp.999.1");
        std::fs::write(&stale, vec![b'x'; 2 * 1024 * 1024]).unwrap();
        set_mtime_secs_ago(&stale, STALE_TEMP_AGE.as_secs() * 2);

        let cfg = RetentionConfig {
            retention_days: 7,
            max_size_mb: 1,
            retention_secs_override: None,
        };
        let stats = prune(&db, current_project.id, &cfg, false).unwrap();

        assert!(
            !stale.exists(),
            "a stale temp under another project must be reaped"
        );
        assert!(
            stats.bytes_freed >= 2 * 1024 * 1024,
            "cross-project temp bytes must be reported as freed"
        );
        assert!(
            total_disk_usage().unwrap() <= cfg.max_size_bytes(),
            "reaping stale temps should bring the global store back under the cap"
        );
    }

    #[test]
    fn prune_waits_for_snapshot_reference_to_commit() {
        use std::sync::mpsc;
        use std::time::Duration;

        let data_dir = tempfile::tempdir().unwrap();
        let data_path = data_dir.path().to_path_buf();
        crate::set_test_data_dir(data_path.clone());
        let db = Database::open().unwrap();
        let project = db.get_or_create_project(Path::new("/proj")).unwrap();
        let project_id = project.id;
        let content = b"new content that must survive concurrent prune".to_vec();
        let hash = crate::snapshots::hash_bytes(&content);
        let expected_path = crate::snapshots::snapshot_path(project_id, &hash).unwrap();

        let (published_tx, published_rx) = mpsc::channel();
        let (commit_tx, commit_rx) = mpsc::channel();
        let publisher_data_path = data_path.clone();
        let publisher_hash = hash.clone();
        let publisher = std::thread::spawn(move || {
            crate::set_test_data_dir(publisher_data_path);
            let db = Database::open().unwrap();
            let guard = crate::snapshots::acquire_publish_guard().unwrap();
            let snapshot =
                crate::snapshots::save_durable(&guard, project_id, &publisher_hash, &content)
                    .unwrap();
            published_tx.send(()).unwrap();
            commit_rx.recv().unwrap();
            db.transaction(|db| {
                db.insert_event(
                    project_id,
                    "/proj/new.rs",
                    "CREATED",
                    Some(&publisher_hash),
                    None,
                    Some(&snapshot),
                    None,
                    Some(content.len() as i64),
                )?;
                db.upsert_file_state(
                    project_id,
                    "/proj/new.rs",
                    &publisher_hash,
                    true,
                    content.len() as i64,
                    None,
                )?;
                Ok(())
            })
            .unwrap();
        });

        published_rx.recv().unwrap();
        assert!(
            expected_path.exists(),
            "publisher must expose the snapshot before committing its reference"
        );

        let (prune_started_tx, prune_started_rx) = mpsc::channel();
        let (prune_finished_tx, prune_finished_rx) = mpsc::channel();
        let pruner_data_path = data_path.clone();
        let pruner = std::thread::spawn(move || {
            crate::set_test_data_dir(pruner_data_path);
            let db = Database::open().unwrap();
            prune_started_tx.send(()).unwrap();
            let result = prune(&db, project_id, &RetentionConfig::default(), false);
            prune_finished_tx.send(result).unwrap();
        });

        prune_started_rx.recv().unwrap();
        assert!(
            prune_finished_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "prune must wait while a published snapshot has no committed reference"
        );

        commit_tx.send(()).unwrap();
        publisher.join().unwrap();
        prune_finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("prune did not finish after the publisher committed")
            .unwrap();
        pruner.join().unwrap();

        assert!(
            expected_path.exists(),
            "prune deleted a snapshot whose reference committed concurrently"
        );
        assert!(db.get_live_hashes(project_id).unwrap().contains(&hash));
    }

    /// Backdate a file's mtime by `secs_ago` seconds via `utimes(2)` so the
    /// staleness check in prune can be exercised deterministically without
    /// sleeping. Unix-only, which matches the rest of the crate.
    fn set_mtime_secs_ago(path: &Path, secs_ago: u64) {
        use std::os::unix::ffi::OsStrExt;
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let tv = libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes must succeed to set up the test");
    }
}
