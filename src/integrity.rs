//! Snapshot-store integrity checks.
//!
//! The snapshot files and the SQLite event/state rows live in two separate
//! durability domains, so a power loss (or a hand-edited `~/.undo`) can in
//! principle leave a `file_state` row whose backing snapshot is missing or — in
//! the deep check — fails to decompress. The write paths are ordered to make
//! this self-healing where possible (snapshots are written before the rows that
//! reference them, and the live path now fsyncs), but nothing previously made
//! such a gap *visible*. This module verifies the snapshots referenced by the
//! current on-disk state and reports problems; it never deletes history.

use anyhow::Result;
use std::collections::BTreeSet;

use crate::db::Database;
use crate::snapshots;

/// Outcome of an integrity check over one project's current-state snapshots.
#[derive(Debug, Default, Clone, Copy)]
pub struct IntegrityReport {
    /// Distinct snapshots that should back a currently-existing file.
    pub checked: usize,
    /// Referenced snapshots whose file is absent from the store.
    pub missing: usize,
    /// Referenced snapshots present but undecodable (only counted when `deep`).
    pub corrupt: usize,
}

impl IntegrityReport {
    pub fn is_clean(&self) -> bool {
        self.missing == 0 && self.corrupt == 0
    }
}

/// Verify the snapshots referenced by files that currently exist on disk
/// (`file_state.latest_hash` where `exists_now = 1`).
///
/// `deep = false` checks only existence (a cheap `stat` per distinct hash) — the
/// power-loss symptom of a committed row pointing at a snapshot whose bytes never
/// landed. It is fast enough to run at daemon startup without regressing the
/// scan. `deep = true` additionally decompresses each snapshot (CRC-checking it),
/// catching truncation/corruption; it costs a full read per snapshot, so it is
/// reserved for the user-invoked `undo status`.
///
/// Read-only: it reports counts and never removes or rewrites anything.
pub fn check(db: &Database, project_id: i64, deep: bool) -> Result<IntegrityReport> {
    let states = db.get_all_file_states(project_id)?;

    // Distinct hashes, so files that share content (dedup) are checked once.
    let mut hashes: BTreeSet<&str> = BTreeSet::new();
    for s in &states {
        if s.exists_now
            && let Some(h) = s.latest_hash.as_deref()
        {
            hashes.insert(h);
        }
    }

    let mut report = IntegrityReport::default();
    for hash in hashes {
        report.checked += 1;
        let path = snapshots::snapshot_path(project_id, hash)?;
        if !path.exists() {
            report.missing += 1;
            continue;
        }
        if deep && snapshots::load(project_id, hash).is_err() {
            report.corrupt += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn setup() -> (tempfile::TempDir, Database, i64) {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let db = Database::open_in_memory().unwrap();
        let pid = db
            .get_or_create_project(std::path::Path::new("/proj"))
            .unwrap();
        (data_dir, db, pid.id)
    }

    /// A store whose every referenced snapshot is present (and decodable) reports
    /// clean, and counts each distinct hash exactly once.
    #[test]
    fn check_reports_clean_store() {
        let (_d, db, pid) = setup();
        snapshots::save(pid, "h1", b"alpha").unwrap();
        snapshots::save(pid, "h2", b"beta").unwrap();
        db.upsert_file_state(pid, "/proj/a.rs", "h1", true, 5, None)
            .unwrap();
        db.upsert_file_state(pid, "/proj/b.rs", "h2", true, 4, None)
            .unwrap();
        // A third file sharing a.rs's content must not double-count the hash.
        db.upsert_file_state(pid, "/proj/c.rs", "h1", true, 5, None)
            .unwrap();

        let report = check(&db, pid, true).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.checked, 2, "two distinct hashes");
    }

    /// A current file whose backing snapshot is absent is counted as missing —
    /// the power-loss symptom — and is caught even by the cheap (non-deep) check.
    #[test]
    fn check_detects_missing_snapshot() {
        let (_d, db, pid) = setup();
        db.upsert_file_state(pid, "/proj/gone.rs", "absent_hash", true, 1, None)
            .unwrap();

        let report = check(&db, pid, false).unwrap();
        assert_eq!(report.checked, 1);
        assert_eq!(report.missing, 1);
        assert_eq!(report.corrupt, 0);
        assert!(!report.is_clean());
    }

    /// A truncated/garbage snapshot file exists but cannot be decompressed. The
    /// deep check flags it as corrupt; the cheap check (existence only) does not.
    #[test]
    fn deep_check_detects_corrupt_snapshot_but_shallow_does_not() {
        let (_d, db, pid) = setup();
        db.upsert_file_state(pid, "/proj/bad.rs", "corrupt_hash", true, 3, None)
            .unwrap();
        // Write a non-gzip file at the snapshot path so load() fails the CRC/format.
        let path = snapshots::snapshot_path(pid, "corrupt_hash").unwrap();
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"this is not gzip").unwrap();

        let shallow = check(&db, pid, false).unwrap();
        assert_eq!(shallow.missing, 0, "file exists, so not missing");
        assert_eq!(shallow.corrupt, 0, "shallow check does not decompress");

        let deep = check(&db, pid, true).unwrap();
        assert_eq!(deep.corrupt, 1, "deep check must catch the bad snapshot");
        assert!(!deep.is_clean());
    }

    /// Snapshots referenced only by *deleted* files are not part of the
    /// current-state check — they're allowed to age out via retention, so a
    /// missing one is not an integrity problem.
    #[test]
    fn check_ignores_deleted_file_state() {
        let (_d, db, pid) = setup();
        db.upsert_file_state(pid, "/proj/old.rs", "deleted_hash", true, 1, None)
            .unwrap();
        db.mark_deleted(pid, "/proj/old.rs").unwrap();

        let report = check(&db, pid, true).unwrap();
        assert_eq!(report.checked, 0, "deleted files are not current state");
        assert!(report.is_clean());
    }
}
