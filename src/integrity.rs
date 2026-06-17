//! Startup integrity verification (#41).
//!
//! Every snapshot still referenced by a project's events or live `file_state`
//! should exist on disk and decompress cleanly. This module verifies that and
//! *only reports* — it never deletes or rewrites history, so a transient mount
//! problem, a power-loss-torn snapshot, or a manually removed `~/.undo` file
//! becomes visible (logged + counted in `undo status`) instead of silently
//! surfacing later as a failed `restore`/`diff`.
//!
//! Cost scales with the number of distinct live snapshots (the gzip CRC is checked
//! by fully decompressing each). It runs once at daemon startup and on demand from
//! `undo status`, never on the per-event hot path.

use anyhow::Result;

use crate::db::Database;
use crate::snapshots;

/// Outcome of verifying a project's referenced snapshots.
#[derive(Debug, Default, Clone, Copy)]
pub struct IntegrityReport {
    /// Distinct snapshots referenced and checked.
    pub checked: usize,
    /// Referenced snapshots with no backing file on disk.
    pub missing: usize,
    /// Referenced snapshots present on disk but unreadable (failed to decompress).
    pub corrupt: usize,
}

impl IntegrityReport {
    /// Total unreadable snapshots (missing + corrupt).
    pub fn problems(&self) -> usize {
        self.missing + self.corrupt
    }

    /// True when every referenced snapshot is present and readable.
    pub fn is_clean(&self) -> bool {
        self.problems() == 0
    }
}

/// Verify every snapshot referenced by `project_id` exists and decompresses. The
/// reference set is exactly the live set retention preserves
/// ([`Database::get_live_hashes`]): current-event hashes, the `latest_hash` of
/// files that still exist, and the `previous_hash` of surviving DELETED events.
///
/// When `log_problems` is true each problem is logged via `log_warn!` (teeing to
/// stderr + the daemon log); `undo status` passes false and renders a single
/// summary line instead. Returns the tallied report; it never mutates state.
pub fn verify_project(
    db: &Database,
    project_id: i64,
    log_problems: bool,
) -> Result<IntegrityReport> {
    let hashes = db.get_live_hashes(project_id)?;
    let mut report = IntegrityReport::default();

    for hash in hashes {
        report.checked += 1;
        let path = snapshots::snapshot_path(project_id, &hash)?;
        if !path.exists() {
            report.missing += 1;
            if log_problems {
                crate::log_warn!(
                    "integrity: snapshot {} (project {}) is referenced but missing on disk — \
                     versions backed by it cannot be restored",
                    hash,
                    project_id
                );
            }
            continue;
        }
        if let Err(e) = snapshots::load(project_id, &hash) {
            report.corrupt += 1;
            if log_problems {
                crate::log_warn!(
                    "integrity: snapshot {} (project {}) failed to decompress: {} — \
                     versions backed by it cannot be restored",
                    hash,
                    project_id,
                    e
                );
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn setup() -> (tempfile::TempDir, Database, i64) {
        let dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(dir.path().to_path_buf());
        let db = Database::open_in_memory().unwrap();
        let p = db.get_or_create_project(Path::new("/proj")).unwrap();
        (dir, db, p.id)
    }

    /// A clean project (every referenced snapshot present and readable) reports no
    /// problems, with `checked` reflecting the referenced set.
    #[test]
    fn verify_passes_when_snapshots_present() {
        let (_dir, db, pid) = setup();
        let content = b"present and readable";
        snapshots::save_durable(pid, "good_hash", content).unwrap();
        db.insert_event(
            pid,
            "/proj/a.rs",
            "CREATED",
            Some("good_hash"),
            None,
            None,
            None,
            Some(content.len() as i64),
        )
        .unwrap();

        let report = verify_project(&db, pid, false).unwrap();
        assert_eq!(report.checked, 1);
        assert!(report.is_clean(), "expected clean report: {report:?}");
    }

    /// An event referencing a hash with no snapshot file on disk is counted as
    /// missing, and history is left untouched (report-only).
    #[test]
    fn verify_flags_missing_snapshot() {
        let (_dir, db, pid) = setup();
        db.insert_event(
            pid,
            "/proj/a.rs",
            "CREATED",
            Some("ghost_hash"),
            None,
            None,
            None,
            Some(1),
        )
        .unwrap();

        let report = verify_project(&db, pid, false).unwrap();
        assert_eq!(report.checked, 1);
        assert_eq!(report.missing, 1);
        assert_eq!(report.corrupt, 0);
        assert!(!report.is_clean());
        // The event itself must survive an integrity check — it only reports.
        assert_eq!(db.count_events(pid).unwrap(), 1);
    }

    /// A snapshot file that exists but is not valid gzip is counted as corrupt
    /// (caught by the decompress/CRC step), distinct from missing.
    #[test]
    fn verify_flags_corrupt_snapshot() {
        let (_dir, db, pid) = setup();
        let path = snapshots::snapshot_path(pid, "corrupt_hash").unwrap();
        std::fs::write(&path, b"this is not a gzip stream").unwrap();
        db.insert_event(
            pid,
            "/proj/a.rs",
            "CREATED",
            Some("corrupt_hash"),
            None,
            None,
            None,
            Some(1),
        )
        .unwrap();

        let report = verify_project(&db, pid, false).unwrap();
        assert_eq!(report.checked, 1);
        assert_eq!(report.missing, 0);
        assert_eq!(report.corrupt, 1);
    }
}
