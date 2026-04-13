use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

use crate::models::{FileEvent, FileState, WatchedProject};

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open() -> Result<Self> {
        let dir = crate::backtrack_dir()?;
        let db_path = dir.join("database.db");
        let conn =
            Connection::open(&db_path).context("failed to open database")?;

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
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id  INTEGER NOT NULL,
                path        TEXT    NOT NULL,
                latest_hash TEXT,
                last_seen_at INTEGER NOT NULL,
                exists_now  INTEGER NOT NULL DEFAULT 1,
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

        Ok(Self { conn })
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
