use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;

use crate::models::{FileEvent, FileState, WatchedProject};

pub struct Database {
    conn: Connection,
}

fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS watched_projects (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            root_path  TEXT    NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS file_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id    INTEGER NOT NULL,
            timestamp     INTEGER NOT NULL,
            path          TEXT    NOT NULL,
            event_type    TEXT    NOT NULL,
            current_hash  TEXT,
            previous_hash TEXT,
            snapshot_path TEXT,
            old_path      TEXT,
            file_size     INTEGER,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id)
        );

        CREATE TABLE IF NOT EXISTS file_state (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id   INTEGER NOT NULL,
            path         TEXT    NOT NULL,
            latest_hash  TEXT,
            last_seen_at INTEGER NOT NULL,
            exists_now   INTEGER NOT NULL DEFAULT 1,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            UNIQUE(project_id, path)
        );

        CREATE INDEX IF NOT EXISTS idx_events_project_time
            ON file_events(project_id, timestamp);
        CREATE INDEX IF NOT EXISTS idx_events_path
            ON file_events(project_id, path, timestamp);
        CREATE INDEX IF NOT EXISTS idx_state_project_path
            ON file_state(project_id, path);",
    )?;
    Ok(())
}

impl Database {
    pub fn open() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::backtrack_dir()?;
        let db_path = dir.join("database.db");
        let conn =
            Connection::open(&db_path).context("failed to open database")?;
        apply_schema(&conn)?;
        // Restrict DB file to owner-only (0600)
        let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("failed to open in-memory database")?;
        apply_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Insert an event with an explicit timestamp. Test-only helper used by
    /// retention tests that need to seed events at controlled points in time.
    #[cfg(test)]
    pub fn insert_event_at(
        &self,
        project_id: i64,
        path: &str,
        event_type: &str,
        ts: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO file_events (project_id, timestamp, path, event_type)
             VALUES (?1, ?2, ?3, ?4)",
            params![project_id, ts, path, event_type],
        )?;
        Ok(())
    }

    // ── project operations ──────────────────────────────────────────

    pub fn get_or_create_project(&self, root_path: &Path) -> Result<WatchedProject> {
        let path_str = root_path.to_string_lossy().to_string();
        let now = Utc::now().timestamp();

        self.conn.execute(
            "INSERT OR IGNORE INTO watched_projects (root_path, created_at)
             VALUES (?1, ?2)",
            params![path_str, now],
        )?;

        let project = self.conn.query_row(
            "SELECT id, root_path, created_at
             FROM watched_projects WHERE root_path = ?1",
            params![path_str],
            |row| {
                Ok(WatchedProject {
                    id: row.get(0)?,
                    root_path: row.get(1)?,
                    created_at: row.get(2)?,
                })
            },
        )?;

        Ok(project)
    }

    pub fn find_project_for_path(
        &self,
        path: &Path,
    ) -> Result<Option<WatchedProject>> {
        let path_str = path.to_string_lossy().to_string();
        // ORDER BY LENGTH DESC ensures the most specific (longest) root_path
        // wins when multiple watched projects are nested inside one another —
        // e.g. watching both /a/b and /a/b/c and querying from /a/b/c/src
        // should resolve to /a/b/c, not /a/b.
        // SUBSTR prefix check is used instead of LIKE to avoid case-folding on
        // case-sensitive filesystems.
        self.conn
            .query_row(
                "SELECT id, root_path, created_at
                 FROM watched_projects
                 WHERE ?1 = root_path
                    OR SUBSTR(?1, 1, LENGTH(root_path) + 1) = root_path || '/'
                 ORDER BY LENGTH(root_path) DESC
                 LIMIT 1",
                rusqlite::params![path_str],
                |row| {
                    Ok(WatchedProject {
                        id: row.get(0)?,
                        root_path: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .context("failed to query project for path")
    }

    // ── event operations ────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn insert_event(
        &self,
        project_id: i64,
        path: &str,
        event_type: &str,
        current_hash: Option<&str>,
        previous_hash: Option<&str>,
        snapshot_path: Option<&str>,
        old_path: Option<&str>,
        file_size: Option<i64>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO file_events
                (project_id, timestamp, path, event_type,
                 current_hash, previous_hash, snapshot_path, old_path, file_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                project_id,
                now,
                path,
                event_type,
                current_hash,
                previous_hash,
                snapshot_path,
                old_path,
                file_size
            ],
        )?;
        Ok(())
    }

    pub fn get_timeline(
        &self,
        project_id: i64,
        limit: usize,
    ) -> Result<Vec<FileEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, timestamp, path, event_type,
                    current_hash, previous_hash, snapshot_path, old_path, file_size
             FROM file_events
             WHERE project_id = ?1
             ORDER BY timestamp DESC
             LIMIT ?2",
        )?;
        let events = stmt.query_map(params![project_id, limit as i64], row_to_event)?;
        events
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query timeline")
    }

    pub fn get_events_since(
        &self,
        project_id: i64,
        since_timestamp: i64,
    ) -> Result<Vec<FileEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, timestamp, path, event_type,
                    current_hash, previous_hash, snapshot_path, old_path, file_size
             FROM file_events
             WHERE project_id = ?1 AND timestamp >= ?2
             ORDER BY timestamp DESC",
        )?;
        let events =
            stmt.query_map(params![project_id, since_timestamp], row_to_event)?;
        events
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query events")
    }

    pub fn get_latest_event(
        &self,
        project_id: i64,
        path: &str,
    ) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1 AND path = ?2
                 ORDER BY timestamp DESC
                 LIMIT 1",
                params![project_id, path],
                row_to_event,
            )
            .optional()
            .context("failed to query latest event")
    }

    /// Find the most recent restorable event at or before `before_ts`.
    pub fn get_event_at_time(
        &self,
        project_id: i64,
        path: &str,
        before_ts: i64,
    ) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1
                   AND path = ?2
                   AND timestamp <= ?3
                   AND event_type != 'DELETED'
                 ORDER BY timestamp DESC
                 LIMIT 1",
                params![project_id, path, before_ts],
                row_to_event,
            )
            .optional()
            .context("failed to query event at time")
    }

    /// Find the oldest non-DELETE event for a file (the earliest known state).
    pub fn get_oldest_event(
        &self,
        project_id: i64,
        path: &str,
    ) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1
                   AND path = ?2
                   AND event_type != 'DELETED'
                 ORDER BY timestamp ASC
                 LIMIT 1",
                params![project_id, path],
                row_to_event,
            )
            .optional()
            .context("failed to query oldest event")
    }

    pub fn count_events(&self, project_id: i64) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM file_events WHERE project_id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .context("failed to count events")
    }

    // ── file state operations ───────────────────────────────────────

    pub fn upsert_file_state(
        &self,
        project_id: i64,
        path: &str,
        hash: &str,
        exists: bool,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO file_state (project_id, path, latest_hash, last_seen_at, exists_now)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(project_id, path) DO UPDATE SET
                latest_hash  = excluded.latest_hash,
                last_seen_at = excluded.last_seen_at,
                exists_now   = excluded.exists_now",
            params![project_id, path, hash, now, exists as i32],
        )?;
        Ok(())
    }

    pub fn get_file_state(
        &self,
        project_id: i64,
        path: &str,
    ) -> Result<Option<FileState>> {
        self.conn
            .query_row(
                "SELECT id, project_id, path, latest_hash, last_seen_at, exists_now
                 FROM file_state
                 WHERE project_id = ?1 AND path = ?2",
                params![project_id, path],
                |row| {
                    Ok(FileState {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        path: row.get(2)?,
                        latest_hash: row.get(3)?,
                        last_seen_at: row.get(4)?,
                        exists_now: row.get::<_, i32>(5)? != 0,
                    })
                },
            )
            .optional()
            .context("failed to query file state")
    }

    pub fn mark_deleted(&self, project_id: i64, path: &str) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "UPDATE file_state SET exists_now = 0, last_seen_at = ?1
             WHERE project_id = ?2 AND path = ?3",
            params![now, project_id, path],
        )?;
        Ok(())
    }

    pub fn get_all_file_states(
        &self,
        project_id: i64,
    ) -> Result<Vec<FileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, path, latest_hash, last_seen_at, exists_now
             FROM file_state
             WHERE project_id = ?1",
        )?;
        let states = stmt.query_map(params![project_id], |row| {
            Ok(FileState {
                id: row.get(0)?,
                project_id: row.get(1)?,
                path: row.get(2)?,
                latest_hash: row.get(3)?,
                last_seen_at: row.get(4)?,
                exists_now: row.get::<_, i32>(5)? != 0,
            })
        })?;
        states
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query file states")
    }

    // ── retention / pruning ──────────────────────────────────────────

    pub fn count_events_before(
        &self,
        project_id: i64,
        before_ts: i64,
    ) -> Result<u64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM file_events
                 WHERE project_id = ?1 AND timestamp < ?2",
                params![project_id, before_ts],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c as u64)
            .context("failed to count events before timestamp")
    }

    pub fn delete_events_before(
        &self,
        project_id: i64,
        before_ts: i64,
    ) -> Result<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM file_events
             WHERE project_id = ?1 AND timestamp < ?2",
            params![project_id, before_ts],
        )?;
        Ok(deleted as u64)
    }

    pub fn get_live_hashes(&self, project_id: i64) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT current_hash FROM file_events
             WHERE project_id = ?1 AND current_hash IS NOT NULL",
        )?;
        let hashes = stmt.query_map(params![project_id], |row| row.get::<_, String>(0))?;
        hashes
            .collect::<Result<HashSet<_>, _>>()
            .context("failed to query live hashes")
    }

    pub fn get_all_project_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM watched_projects")?;
        let ids = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        ids.collect::<Result<Vec<_>, _>>()
            .context("failed to query project ids")
    }
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<FileEvent> {
    Ok(FileEvent {
        id: row.get(0)?,
        project_id: row.get(1)?,
        timestamp: row.get(2)?,
        path: row.get(3)?,
        event_type: row.get(4)?,
        current_hash: row.get(5)?,
        previous_hash: row.get(6)?,
        snapshot_path: row.get(7)?,
        old_path: row.get(8)?,
        file_size: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── helpers ──────────────────────────────────────────────────────

    fn db() -> Database {
        Database::open_in_memory().expect("in-memory DB")
    }

    fn project(db: &Database) -> crate::models::WatchedProject {
        db.get_or_create_project(Path::new("/home/user/project"))
            .expect("create project")
    }

    // ── watched_projects ─────────────────────────────────────────────

    #[test]
    fn create_project_stores_root_path() {
        let db = db();
        let p = project(&db);
        assert_eq!(p.root_path, "/home/user/project");
    }

    #[test]
    fn create_project_is_idempotent() {
        let db = db();
        let p1 = project(&db);
        let p2 = project(&db);
        assert_eq!(p1.id, p2.id);
    }

    #[test]
    fn find_project_exact_match() {
        let db = db();
        let created = project(&db);
        let found = db
            .find_project_for_path(Path::new("/home/user/project"))
            .unwrap()
            .unwrap();
        assert_eq!(found.id, created.id);
    }

    #[test]
    fn find_project_subdirectory_match() {
        let db = db();
        let created = project(&db);
        let found = db
            .find_project_for_path(Path::new("/home/user/project/src/main.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(found.id, created.id);
    }

    #[test]
    fn find_project_returns_none_for_unrelated_path() {
        let db = db();
        project(&db);
        let result = db
            .find_project_for_path(Path::new("/other/entirely/different"))
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_project_no_false_positive_for_shared_string_prefix() {
        // "/home/user/project-evil" shares the string prefix "/home/user/project"
        // but is NOT a subdirectory of it — must not match.
        let db = db();
        project(&db);
        let result = db
            .find_project_for_path(Path::new("/home/user/project-evil"))
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_project_returns_most_specific_nested_match() {
        let db = db();
        let parent = db
            .get_or_create_project(Path::new("/a/b"))
            .unwrap();
        let child = db
            .get_or_create_project(Path::new("/a/b/c"))
            .unwrap();
        let found = db
            .find_project_for_path(Path::new("/a/b/c/src/main.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(found.id, child.id);
        assert_ne!(found.id, parent.id);
    }

    // ── file_events ──────────────────────────────────────────────────

    #[test]
    fn insert_events_and_count() {
        let db = db();
        let p = project(&db);
        db.insert_event(p.id, "/home/user/project/a.rs", "CREATED",
            Some("aaa"), None, None, None, Some(10)).unwrap();
        db.insert_event(p.id, "/home/user/project/b.rs", "MODIFIED",
            Some("bbb"), Some("bbb0"), None, None, Some(20)).unwrap();
        assert_eq!(db.count_events(p.id).unwrap(), 2);
    }

    // ── file_state ───────────────────────────────────────────────────

    #[test]
    fn upsert_and_retrieve_file_state() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/main.rs";
        db.upsert_file_state(p.id, path, "deadbeef", true).unwrap();
        let state = db.get_file_state(p.id, path).unwrap().unwrap();
        assert_eq!(state.latest_hash, Some("deadbeef".to_string()));
        assert!(state.exists_now);
    }

    // ── retention methods ───────────────────────────────────────────

    fn seed_events(db: &Database, project_id: i64) {
        let now = chrono::Utc::now().timestamp();
        // Old event: 10 days ago
        db.conn.execute(
            "INSERT INTO file_events (project_id, timestamp, path, event_type, current_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project_id, now - 864_000, "/p/old.rs", "MODIFIED", "hash_old"],
        ).unwrap();
        // Recent event: 1 hour ago
        db.conn.execute(
            "INSERT INTO file_events (project_id, timestamp, path, event_type, current_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project_id, now - 3600, "/p/new.rs", "MODIFIED", "hash_new"],
        ).unwrap();
    }

    #[test]
    fn count_events_before_counts_old_events() {
        let db = db();
        let p = project(&db);
        seed_events(&db, p.id);
        let now = chrono::Utc::now().timestamp();
        let cutoff = now - 86400; // 1 day ago
        assert_eq!(db.count_events_before(p.id, cutoff).unwrap(), 1);
    }

    #[test]
    fn delete_events_before_removes_old_events() {
        let db = db();
        let p = project(&db);
        seed_events(&db, p.id);
        let now = chrono::Utc::now().timestamp();
        let cutoff = now - 86400;
        let deleted = db.delete_events_before(p.id, cutoff).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(db.count_events(p.id).unwrap(), 1);
    }

    #[test]
    fn get_live_hashes_returns_referenced_hashes() {
        let db = db();
        let p = project(&db);
        seed_events(&db, p.id);
        let hashes = db.get_live_hashes(p.id).unwrap();
        assert!(hashes.contains("hash_old"));
        assert!(hashes.contains("hash_new"));
        assert_eq!(hashes.len(), 2);
    }

    #[test]
    fn get_live_hashes_after_prune_excludes_deleted() {
        let db = db();
        let p = project(&db);
        seed_events(&db, p.id);
        let now = chrono::Utc::now().timestamp();
        db.delete_events_before(p.id, now - 86400).unwrap();
        let hashes = db.get_live_hashes(p.id).unwrap();
        assert!(!hashes.contains("hash_old"));
        assert!(hashes.contains("hash_new"));
    }

    #[test]
    fn get_all_project_ids_returns_existing_projects() {
        let db = db();
        let p1 = db.get_or_create_project(Path::new("/a")).unwrap();
        let p2 = db.get_or_create_project(Path::new("/b")).unwrap();
        let ids = db.get_all_project_ids().unwrap();
        assert!(ids.contains(&p1.id));
        assert!(ids.contains(&p2.id));
    }

    #[test]
    fn get_latest_event_returns_most_recent() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/foo.rs";
        // Insert two events for the same path via insert_event_at so we control ordering.
        let now = chrono::Utc::now().timestamp();
        db.insert_event_at(p.id, path, "CREATED", now - 100).unwrap();
        db.insert_event_at(p.id, path, "MODIFIED", now - 10).unwrap();
        let event = db.get_latest_event(p.id, path).unwrap().unwrap();
        assert_eq!(event.event_type, "MODIFIED");
    }

    #[test]
    fn get_event_at_time_excludes_deleted_and_respects_cutoff() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/bar.rs";
        let now = chrono::Utc::now().timestamp();
        // Seed: CREATED long ago, MODIFIED in the middle, DELETED recently.
        db.insert_event_at(p.id, path, "CREATED", now - 300).unwrap();
        db.insert_event_at(p.id, path, "MODIFIED", now - 200).unwrap();
        db.insert_event_at(p.id, path, "DELETED", now - 100).unwrap();

        // Querying at now-150 should return MODIFIED (newest non-DELETE at or before that point).
        let event = db.get_event_at_time(p.id, path, now - 150).unwrap().unwrap();
        assert_eq!(event.event_type, "MODIFIED");

        // Querying before any event returns None.
        let none = db.get_event_at_time(p.id, path, now - 1000).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn mark_deleted_sets_exists_now_false() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/gone.rs";
        db.upsert_file_state(p.id, path, "abc123", true).unwrap();
        // Confirm it's alive before we delete it.
        assert!(db.get_file_state(p.id, path).unwrap().unwrap().exists_now);
        db.mark_deleted(p.id, path).unwrap();
        let state = db.get_file_state(p.id, path).unwrap().unwrap();
        assert!(!state.exists_now);
    }
}
