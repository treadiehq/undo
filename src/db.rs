use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::path::Path;

use crate::models::{Checkpoint, FileEvent, FileState, Session, WatchedProject};

pub struct Database {
    conn: Connection,
}

// Debug-only reentrancy guard for `Database::transaction`. The daemon drives its
// single connection from one thread, so a thread-local flag is enough to catch a
// nested `BEGIN` (a programming error) without touching the struct's constructors.
#[cfg(debug_assertions)]
thread_local! {
    static IN_TRANSACTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
            size         INTEGER,
            mtime_nanos  INTEGER,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            UNIQUE(project_id, path)
        );

        CREATE TABLE IF NOT EXISTS checkpoints (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id INTEGER NOT NULL,
            name       TEXT    NOT NULL,
            timestamp  INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            UNIQUE(project_id, name)
        );

        CREATE TABLE IF NOT EXISTS sessions (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id INTEGER NOT NULL,
            name       TEXT    NOT NULL,
            kind       TEXT    NOT NULL,
            started_at INTEGER NOT NULL,
            ended_at   INTEGER,
            start_event_id INTEGER NOT NULL DEFAULT 0,
            end_event_id   INTEGER,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            UNIQUE(project_id, name)
        );

        CREATE TABLE IF NOT EXISTS session_events (
            session_id INTEGER NOT NULL,
            event_id   INTEGER NOT NULL,
            PRIMARY KEY (session_id, event_id),
            FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY (event_id) REFERENCES file_events(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_events_project_time
            ON file_events(project_id, timestamp);
        CREATE INDEX IF NOT EXISTS idx_events_path
            ON file_events(project_id, path, timestamp);
        CREATE INDEX IF NOT EXISTS idx_state_project_path
            ON file_state(project_id, path);
        CREATE INDEX IF NOT EXISTS idx_checkpoints_project_time
            ON checkpoints(project_id, timestamp);
        CREATE INDEX IF NOT EXISTS idx_sessions_project_time
            ON sessions(project_id, started_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_one_active
            ON sessions(project_id) WHERE ended_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_session_events_event
            ON session_events(event_id);",
    )?;

    // Additive migration (#26): databases created before size/mtime tracking
    // lack these columns. New databases already have them from the CREATE TABLE
    // above, so these ALTERs fail with "duplicate column name" — which is the
    // expected, harmless outcome we deliberately ignore.
    for stmt in [
        "ALTER TABLE file_state ADD COLUMN size INTEGER",
        "ALTER TABLE file_state ADD COLUMN mtime_nanos INTEGER",
    ] {
        let _ = conn.execute(stmt, []);
    }
    for stmt in [
        "ALTER TABLE sessions ADD COLUMN start_event_id INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE sessions ADD COLUMN end_event_id INTEGER",
    ] {
        let _ = conn.execute(stmt, []);
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS checkpoints (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id INTEGER NOT NULL,
            name       TEXT    NOT NULL,
            timestamp  INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            UNIQUE(project_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_checkpoints_project_time
            ON checkpoints(project_id, timestamp);
        CREATE TABLE IF NOT EXISTS sessions (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id INTEGER NOT NULL,
            name       TEXT    NOT NULL,
            kind       TEXT    NOT NULL DEFAULT 'manual',
            started_at INTEGER NOT NULL,
            ended_at   INTEGER,
            start_event_id INTEGER NOT NULL DEFAULT 0,
            end_event_id   INTEGER,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            UNIQUE(project_id, name)
        );
        CREATE TABLE IF NOT EXISTS session_events (
            session_id INTEGER NOT NULL,
            event_id   INTEGER NOT NULL,
            PRIMARY KEY (session_id, event_id),
            FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY (event_id) REFERENCES file_events(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_project_time
            ON sessions(project_id, started_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_one_active
            ON sessions(project_id) WHERE ended_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_session_events_event
            ON session_events(event_id);",
    )?;

    Ok(())
}

/// Force `database.db` and its `-wal`/`-shm` sidecars to mode 0600.
/// Best-effort (like the rest of the permission tightening): a chmod failure
/// is non-fatal because the parent `~/.undo` is already 0700.
fn restrict_db_files(db_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db_path.as_os_str().to_os_string();
        p.push(suffix);
        let p = std::path::PathBuf::from(p);
        if p.exists() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}

impl Database {
    pub fn open() -> Result<Self> {
        let dir = crate::backtrack_dir()?;
        let db_path = dir.join("database.db");
        let conn = Connection::open(&db_path).context("failed to open database")?;
        apply_schema(&conn)?;
        // Restrict the DB and its WAL sidecars to owner-only. apply_schema()
        // enables WAL mode, which creates `-wal` and `-shm` — and SQLite
        // creates THOSE with the process umask (typically 0644), even though
        // they hold the same data as the main DB. Lock all three down so the
        // 0600 policy actually covers every file that contains snapshot data,
        // not just `database.db`.
        restrict_db_files(&db_path);
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory database")?;
        apply_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Run `f` inside a single SQLite transaction, committing on `Ok` and rolling
    /// back on `Err`. Batching the initial scan's many inserts into one commit
    /// avoids a per-statement WAL commit on the on-disk database, which dominates
    /// scan time for large repositories. The live-path handlers also use it so an
    /// event and its `file_state` update commit all-or-nothing.
    ///
    /// Not reentrant: it issues raw `BEGIN`/`COMMIT`, so a nested call would emit
    /// `BEGIN` inside an open transaction and SQLite would error. The daemon's
    /// single connection makes nesting a programming error rather than a runtime
    /// condition, so we catch it loudly in debug builds instead of letting it
    /// surface as an opaque SQL error in production.
    pub fn transaction<R>(&self, f: impl FnOnce(&Self) -> Result<R>) -> Result<R> {
        #[cfg(debug_assertions)]
        IN_TRANSACTION.with(|flag| {
            assert!(
                !flag.get(),
                "db.transaction() is not reentrant — a nested call would emit BEGIN within BEGIN"
            );
            flag.set(true);
        });

        let result = (|| {
            self.conn.execute_batch("BEGIN")?;
            match f(self) {
                Ok(value) => {
                    self.conn.execute_batch("COMMIT")?;
                    Ok(value)
                }
                Err(e) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })();

        #[cfg(debug_assertions)]
        IN_TRANSACTION.with(|flag| flag.set(false));

        result
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

    pub fn find_project_for_path(&self, path: &Path) -> Result<Option<WatchedProject>> {
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
        // `prepare_cached` reuses the parsed statement across calls — the initial
        // scan applies tens of thousands of these inside one transaction, so
        // re-parsing the SQL each time would be pure waste (#27).
        self.conn
            .prepare_cached(
                "INSERT INTO file_events
                (project_id, timestamp, path, event_type,
                 current_hash, previous_hash, snapshot_path, old_path, file_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                project_id,
                now,
                path,
                event_type,
                current_hash,
                previous_hash,
                snapshot_path,
                old_path,
                file_size
            ])?;
        Ok(())
    }

    pub fn get_timeline(&self, project_id: i64, limit: usize) -> Result<Vec<FileEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, timestamp, path, event_type,
                    current_hash, previous_hash, snapshot_path, old_path, file_size
             FROM file_events
             WHERE project_id = ?1
             ORDER BY timestamp DESC, id DESC
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
             ORDER BY timestamp DESC, id DESC",
        )?;
        let events = stmt.query_map(params![project_id, since_timestamp], row_to_event)?;
        events
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query events")
    }

    pub fn get_latest_event(&self, project_id: i64, path: &str) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1 AND path = ?2
                 ORDER BY timestamp DESC, id DESC
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
                 ORDER BY timestamp DESC, id DESC
                 LIMIT 1",
                params![project_id, path, before_ts],
                row_to_event,
            )
            .optional()
            .context("failed to query event at time")
    }

    /// Find the newest restorable event at the exact start boundary of a
    /// session. The event id fence matters because watcher events and session
    /// commands are timestamped to whole seconds; events recorded after
    /// `session start` can share the same timestamp as the start command.
    pub fn get_event_at_session_start(
        &self,
        session: &Session,
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
                   AND (
                        timestamp < ?3
                        OR (timestamp = ?3 AND id <= ?4)
                   )
                 ORDER BY timestamp DESC, id DESC
                 LIMIT 1",
                params![
                    session.project_id,
                    path,
                    session.started_at,
                    session.start_event_id
                ],
                row_to_event,
            )
            .optional()
            .context("failed to query event at session start")
    }

    /// Find the most recent DELETED event for a path, if any. A deleted file's
    /// last captured content survives only in the event's `previous_hash`, so
    /// restore uses this as a last resort when no non-DELETE event remains.
    pub fn get_latest_deleted_event(
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
                   AND event_type = 'DELETED'
                 ORDER BY timestamp DESC, id DESC
                 LIMIT 1",
                params![project_id, path],
                row_to_event,
            )
            .optional()
            .context("failed to query latest deleted event")
    }

    /// Find the oldest non-DELETE event for a file (the earliest known state).
    pub fn get_oldest_event(&self, project_id: i64, path: &str) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1
                   AND path = ?2
                   AND event_type != 'DELETED'
                 ORDER BY timestamp ASC, id ASC
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

    pub fn get_deleted_events(&self, project_id: i64, limit: usize) -> Result<Vec<FileEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, timestamp, path, event_type,
                    current_hash, previous_hash, snapshot_path, old_path, file_size
             FROM file_events
             WHERE project_id = ?1
               AND event_type = 'DELETED'
               AND previous_hash IS NOT NULL
             ORDER BY timestamp DESC, id DESC
             LIMIT ?2",
        )?;
        let events = stmt.query_map(params![project_id, limit as i64], row_to_event)?;
        events
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query deleted events")
    }

    // ── file state operations ───────────────────────────────────────

    pub fn upsert_file_state(
        &self,
        project_id: i64,
        path: &str,
        hash: &str,
        exists: bool,
        size: i64,
        mtime_nanos: Option<i64>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn
            .prepare_cached(
                "INSERT INTO file_state (project_id, path, latest_hash, last_seen_at, exists_now, size, mtime_nanos)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(project_id, path) DO UPDATE SET
                latest_hash  = excluded.latest_hash,
                last_seen_at = excluded.last_seen_at,
                exists_now   = excluded.exists_now,
                size         = excluded.size,
                mtime_nanos  = excluded.mtime_nanos",
            )?
            .execute(params![project_id, path, hash, now, exists as i32, size, mtime_nanos])?;
        Ok(())
    }

    pub fn get_file_state(&self, project_id: i64, path: &str) -> Result<Option<FileState>> {
        self.conn
            .query_row(
                "SELECT id, project_id, path, latest_hash, last_seen_at, exists_now, size, mtime_nanos
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
                        size: row.get(6)?,
                        mtime_nanos: row.get(7)?,
                    })
                },
            )
            .optional()
            .context("failed to query file state")
    }

    pub fn mark_deleted(&self, project_id: i64, path: &str) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn
            .prepare_cached(
                "UPDATE file_state SET exists_now = 0, last_seen_at = ?1
             WHERE project_id = ?2 AND path = ?3",
            )?
            .execute(params![now, project_id, path])?;
        Ok(())
    }

    pub fn get_all_file_states(&self, project_id: i64) -> Result<Vec<FileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, path, latest_hash, last_seen_at, exists_now, size, mtime_nanos
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
                size: row.get(6)?,
                mtime_nanos: row.get(7)?,
            })
        })?;
        states
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query file states")
    }

    // ── retention / pruning ──────────────────────────────────────────

    pub fn count_events_before(&self, project_id: i64, before_ts: i64) -> Result<u64> {
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

    pub fn delete_events_before(&self, project_id: i64, before_ts: i64) -> Result<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM file_events
             WHERE project_id = ?1 AND timestamp < ?2",
            params![project_id, before_ts],
        )?;
        Ok(deleted as u64)
    }

    pub fn get_live_hashes(&self, project_id: i64) -> Result<HashSet<String>> {
        // Live = referenced by any current event OR by the latest_hash of a
        // file that still exists on disk. The file_state arm is what keeps a
        // file's snapshot pinned after its creating event predates the
        // retention window — but it MUST be gated on `exists_now = 1`. Without
        // that gate, `mark_deleted` leaves the row's `latest_hash` set, so
        // every file ever deleted permanently anchors its snapshot and
        // retention can never reclaim that disk space.
        //
        // The third arm pins `previous_hash` from surviving events. A DELETED
        // event carries the last captured content only in `previous_hash`; a
        // rename-overwrite can also leave overwritten destination content only
        // in a MODIFIED event's `previous_hash` on Linux. In both cases, the
        // pin lasts exactly as long as the event itself (until
        // `delete_events_before` removes it), so the snapshot is still
        // reclaimed once the event ages past the retention window.
        let mut stmt = self.conn.prepare(
            "SELECT current_hash FROM file_events
             WHERE project_id = ?1 AND current_hash IS NOT NULL
             UNION
             SELECT latest_hash FROM file_state
             WHERE project_id = ?1 AND latest_hash IS NOT NULL
               AND exists_now = 1
             UNION
             SELECT previous_hash FROM file_events
             WHERE project_id = ?1 AND previous_hash IS NOT NULL",
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

    // ── session operations ──────────────────────────────────────────

    pub fn start_session(&self, project_id: i64, name: &str, kind: &str) -> Result<Session> {
        let now = Utc::now().timestamp();
        if let Some(active) = self.get_active_session(project_id)? {
            anyhow::bail!(
                "session '{}' is already active. Stop it before starting another.",
                active.name
            );
        }
        let start_event_id = self.max_event_id(project_id)?;
        self.conn.execute(
            "INSERT INTO sessions
                (project_id, name, kind, started_at, start_event_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![project_id, name, kind, now, start_event_id, now],
        )?;
        self.get_session_by_name(project_id, name)?
            .ok_or_else(|| anyhow::anyhow!("failed to read session after creating it"))
    }

    pub fn stop_active_session(&self, project_id: i64) -> Result<Option<Session>> {
        let Some(session) = self.get_active_session(project_id)? else {
            return Ok(None);
        };
        let ended_at = Utc::now().timestamp();
        let end_event_id = self.max_event_id(project_id)?;
        self.transaction(|db| {
            db.conn.execute(
                "UPDATE sessions SET ended_at = ?1, end_event_id = ?2 WHERE id = ?3",
                params![ended_at, end_event_id, session.id],
            )?;
            db.link_session_events(
                session.id,
                project_id,
                session.started_at,
                ended_at,
                session.start_event_id,
                end_event_id,
            )?;
            Ok(())
        })?;
        self.get_session_by_id(session.id)
    }

    pub fn list_sessions(&self, project_id: i64) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, kind, started_at, ended_at,
                    start_event_id, end_event_id, created_at
             FROM sessions
             WHERE project_id = ?1
             ORDER BY started_at DESC, id DESC",
        )?;
        let sessions = stmt.query_map(params![project_id], row_to_session)?;
        sessions
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query sessions")
    }

    pub fn get_session_by_name(&self, project_id: i64, name: &str) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, kind, started_at, ended_at,
                        start_event_id, end_event_id, created_at
                 FROM sessions
                 WHERE project_id = ?1 AND name = ?2
                 LIMIT 1",
                params![project_id, name],
                row_to_session,
            )
            .optional()
            .context("failed to query session by name")
    }

    pub fn get_active_session(&self, project_id: i64) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, kind, started_at, ended_at,
                        start_event_id, end_event_id, created_at
                 FROM sessions
                 WHERE project_id = ?1 AND ended_at IS NULL
                 ORDER BY started_at DESC, id DESC
                 LIMIT 1",
                params![project_id],
                row_to_session,
            )
            .optional()
            .context("failed to query active session")
    }

    pub fn get_session_events(&self, session: &Session) -> Result<Vec<FileEvent>> {
        let mapped_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session_events WHERE session_id = ?1",
            params![session.id],
            |row| row.get(0),
        )?;

        if mapped_count > 0 {
            let mut stmt = self.conn.prepare(
                "SELECT e.id, e.project_id, e.timestamp, e.path, e.event_type,
                        e.current_hash, e.previous_hash, e.snapshot_path, e.old_path, e.file_size
                 FROM file_events e
                 JOIN session_events se ON se.event_id = e.id
                 WHERE se.session_id = ?1
                 ORDER BY e.timestamp DESC, e.id DESC",
            )?;
            let events = stmt.query_map(params![session.id], row_to_event)?;
            return events
                .collect::<Result<Vec<_>, _>>()
                .context("failed to query mapped session events");
        }

        let ended_at = session.ended_at.unwrap_or_else(|| Utc::now().timestamp());
        let end_event_id = match session.end_event_id {
            Some(id) => id,
            None => self.max_event_id(session.project_id)?,
        };
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, timestamp, path, event_type,
                    current_hash, previous_hash, snapshot_path, old_path, file_size
             FROM file_events
             WHERE project_id = ?1
               AND id > ?2
               AND id <= ?3
               AND timestamp >= ?4
               AND timestamp <= ?5
             ORDER BY timestamp DESC, id DESC",
        )?;
        let events = stmt.query_map(
            params![
                session.project_id,
                session.start_event_id,
                end_event_id,
                session.started_at,
                ended_at
            ],
            row_to_event,
        )?;
        events
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query session events")
    }

    fn get_session_by_id(&self, session_id: i64) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, kind, started_at, ended_at,
                        start_event_id, end_event_id, created_at
                 FROM sessions
                 WHERE id = ?1
                 LIMIT 1",
                params![session_id],
                row_to_session,
            )
            .optional()
            .context("failed to query session by id")
    }

    fn link_session_events(
        &self,
        session_id: i64,
        project_id: i64,
        started_at: i64,
        ended_at: i64,
        start_event_id: i64,
        end_event_id: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_events (session_id, event_id)
             SELECT ?1, id
             FROM file_events
             WHERE project_id = ?2
               AND id > ?3
               AND id <= ?4
               AND timestamp >= ?5
               AND timestamp <= ?6",
            params![
                session_id,
                project_id,
                start_event_id,
                end_event_id,
                started_at,
                ended_at
            ],
        )?;
        Ok(())
    }

    fn max_event_id(&self, project_id: i64) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM file_events WHERE project_id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .context("failed to query max event id")
    }

    // ── checkpoint operations ───────────────────────────────────────

    pub fn create_checkpoint(&self, project_id: i64, name: &str, timestamp: i64) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO checkpoints (project_id, name, timestamp, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project_id, name) DO UPDATE SET
                timestamp  = excluded.timestamp,
                created_at = excluded.created_at",
            params![project_id, name, timestamp, now],
        )?;
        Ok(())
    }

    pub fn list_checkpoints(&self, project_id: i64) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, timestamp, created_at
             FROM checkpoints
             WHERE project_id = ?1
             ORDER BY timestamp DESC, id DESC",
        )?;
        let checkpoints = stmt.query_map(params![project_id], row_to_checkpoint)?;
        checkpoints
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query checkpoints")
    }

    pub fn get_checkpoint(&self, project_id: i64, name: &str) -> Result<Option<Checkpoint>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, timestamp, created_at
                 FROM checkpoints
                 WHERE project_id = ?1 AND name = ?2
                 LIMIT 1",
                params![project_id, name],
                row_to_checkpoint,
            )
            .optional()
            .context("failed to query checkpoint")
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

fn row_to_checkpoint(row: &rusqlite::Row) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        timestamp: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        kind: row.get(3)?,
        started_at: row.get(4)?,
        ended_at: row.get(5)?,
        start_event_id: row.get(6)?,
        end_event_id: row.get(7)?,
        created_at: row.get(8)?,
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

    /// The on-disk DB *and* its WAL sidecars must be owner-only (0600). SQLite
    /// creates `-wal`/`-shm` with the umask (0644 by default), so without the
    /// explicit chmod in `open()` those files — which hold the same snapshot
    /// data as the main DB — would be group/world readable.
    /// (Red before `restrict_db_files`: `-wal`/`-shm` come out 0644.)
    #[test]
    fn open_restricts_db_and_wal_files_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(dir.path().to_path_buf());

        let db = Database::open().expect("open on-disk db");
        // Force a write so the WAL is materialized.
        db.get_or_create_project(Path::new("/x")).unwrap();

        let base = crate::backtrack_dir().unwrap().join("database.db");
        let mut checked_wal = false;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = base.as_os_str().to_os_string();
            p.push(suffix);
            let p = std::path::PathBuf::from(p);
            if p.exists() {
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{:?} must be 0600, got {:o}", p, mode);
                if !suffix.is_empty() {
                    checked_wal = true;
                }
            }
        }
        assert!(
            checked_wal,
            "expected a -wal or -shm sidecar to exist and be checked"
        );
    }

    // ── watched_projects ─────────────────────────────────────────────

    /// Root path is persisted and returned correctly after project creation.
    #[test]
    fn create_project_stores_root_path() {
        let db = db();
        let p = project(&db);
        assert_eq!(p.root_path, "/home/user/project");
    }

    /// Calling get_or_create twice for the same root must return the same project ID.
    #[test]
    fn create_project_is_idempotent() {
        let db = db();
        let p1 = project(&db);
        let p2 = project(&db);
        assert_eq!(p1.id, p2.id);
    }

    /// An exact path match must find the project.
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

    /// A path inside the watched root must resolve to that project.
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

    /// A path with no parent project must return None rather than a wrong match.
    #[test]
    fn find_project_returns_none_for_unrelated_path() {
        let db = db();
        project(&db);
        let result = db
            .find_project_for_path(Path::new("/other/entirely/different"))
            .unwrap();
        assert!(result.is_none());
    }

    /// A path that shares a string prefix but is not a subdirectory must not match.
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

    /// When two projects are nested, the deepest (most specific) match wins.
    #[test]
    fn find_project_returns_most_specific_nested_match() {
        let db = db();
        let parent = db.get_or_create_project(Path::new("/a/b")).unwrap();
        let child = db.get_or_create_project(Path::new("/a/b/c")).unwrap();
        let found = db
            .find_project_for_path(Path::new("/a/b/c/src/main.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(found.id, child.id);
        assert_ne!(found.id, parent.id);
    }

    // ── transactions ─────────────────────────────────────────────────

    /// A transaction that returns `Ok` commits every write inside it.
    #[test]
    fn transaction_commits_all_writes_on_success() {
        let db = db();
        let p = project(&db);
        db.transaction(|db| {
            db.insert_event(
                p.id,
                "/p/a.rs",
                "CREATED",
                Some("h"),
                None,
                None,
                None,
                Some(3),
            )?;
            db.upsert_file_state(p.id, "/p/a.rs", "h", true, 3, Some(1))?;
            Ok(())
        })
        .unwrap();
        assert_eq!(db.count_events(p.id).unwrap(), 1);
        assert!(db.get_file_state(p.id, "/p/a.rs").unwrap().is_some());
    }

    /// A transaction that returns `Err` rolls back ALL of its writes — the failure
    /// window the live handlers close: an event must never be observed without its
    /// matching `file_state` update (and vice versa).
    #[test]
    fn transaction_rolls_back_all_writes_on_error() {
        let db = db();
        let p = project(&db);
        let result: Result<()> = db.transaction(|db| {
            db.insert_event(
                p.id,
                "/p/a.rs",
                "CREATED",
                Some("h"),
                None,
                None,
                None,
                Some(3),
            )?;
            anyhow::bail!("injected failure between the two writes");
        });
        assert!(result.is_err());
        assert_eq!(
            db.count_events(p.id).unwrap(),
            0,
            "the event inserted before the failure must be rolled back"
        );
        assert!(
            db.get_file_state(p.id, "/p/a.rs").unwrap().is_none(),
            "no file_state row should survive the rolled-back transaction"
        );
    }

    /// `transaction` is not reentrant (raw BEGIN/COMMIT). Nesting is a programming
    /// error caught by a debug assertion — verify it fires so the live handlers can
    /// rely on the guard instead of producing an opaque SQL error. Debug-only: the
    /// guard compiles out under `--release`, so this test does too.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "not reentrant")]
    fn nested_transaction_panics_in_debug() {
        let db = db();
        let _ = db.transaction(|db| db.transaction(|_| Ok(())));
    }

    // ── file_events ──────────────────────────────────────────────────

    /// Inserting events increments the count returned by count_events.
    #[test]
    fn insert_events_and_count() {
        let db = db();
        let p = project(&db);
        db.insert_event(
            p.id,
            "/home/user/project/a.rs",
            "CREATED",
            Some("aaa"),
            None,
            None,
            None,
            Some(10),
        )
        .unwrap();
        db.insert_event(
            p.id,
            "/home/user/project/b.rs",
            "MODIFIED",
            Some("bbb"),
            Some("bbb0"),
            None,
            None,
            Some(20),
        )
        .unwrap();
        assert_eq!(db.count_events(p.id).unwrap(), 2);
    }

    /// Same-second events must resolve deterministically to the newest inserted
    /// row, otherwise preview and restore can disagree during rapid edits.
    #[test]
    fn get_event_at_time_breaks_timestamp_ties_by_id() {
        let db = db();
        let p = project(&db);
        let ts = chrono::Utc::now().timestamp();
        for hash in ["first", "second"] {
            db.conn
                .execute(
                    "INSERT INTO file_events
                     (project_id, timestamp, path, event_type, current_hash)
                     VALUES (?1, ?2, ?3, 'MODIFIED', ?4)",
                    params![p.id, ts, "/home/user/project/a.rs", hash],
                )
                .unwrap();
        }

        let event = db
            .get_event_at_time(p.id, "/home/user/project/a.rs", ts)
            .unwrap()
            .unwrap();
        assert_eq!(event.current_hash.as_deref(), Some("second"));
    }

    /// Deleted files with a previous hash are listed as recoverable in newest-first order.
    #[test]
    fn get_deleted_events_returns_recoverable_deletions() {
        let db = db();
        let p = project(&db);
        db.insert_event(
            p.id,
            "/home/user/project/gone.rs",
            "DELETED",
            None,
            Some("gone_hash"),
            None,
            None,
            None,
        )
        .unwrap();
        db.insert_event(
            p.id,
            "/home/user/project/bad.rs",
            "DELETED",
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let deleted = db.get_deleted_events(p.id, 10).unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].path, "/home/user/project/gone.rs");
    }

    // ── checkpoints ──────────────────────────────────────────────────

    /// Checkpoints are labels over timestamps and can be replaced by name.
    #[test]
    fn checkpoint_create_list_and_replace() {
        let db = db();
        let p = project(&db);
        db.create_checkpoint(p.id, "before refactor", 100).unwrap();
        db.create_checkpoint(p.id, "before refactor", 200).unwrap();

        let checkpoint = db
            .get_checkpoint(p.id, "before refactor")
            .unwrap()
            .expect("checkpoint exists");
        assert_eq!(checkpoint.timestamp, 200);

        let checkpoints = db.list_checkpoints(p.id).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].name, "before refactor");
    }

    // ── sessions ─────────────────────────────────────────────────────

    /// Sessions are named windows over events: starting creates one active
    /// window, stopping closes it, and list/show expose the same row.
    #[test]
    fn session_start_stop_list_and_show() {
        let db = db();
        let p = project(&db);

        let started = db.start_session(p.id, "agent-auth-work", "manual").unwrap();
        assert_eq!(started.name, "agent-auth-work");
        assert_eq!(started.kind, "manual");
        assert!(started.ended_at.is_none());

        let active = db.get_active_session(p.id).unwrap().unwrap();
        assert_eq!(active.id, started.id);

        let stopped = db.stop_active_session(p.id).unwrap().unwrap();
        assert_eq!(stopped.id, started.id);
        assert!(stopped.ended_at.is_some());
        assert!(db.get_active_session(p.id).unwrap().is_none());

        let by_name = db
            .get_session_by_name(p.id, "agent-auth-work")
            .unwrap()
            .unwrap();
        assert_eq!(by_name.id, started.id);

        let sessions = db.list_sessions(p.id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "agent-auth-work");
    }

    /// A project can only have one active session; otherwise new events would be
    /// ambiguously attributed.
    #[test]
    fn session_start_rejects_existing_active_session() {
        let db = db();
        let p = project(&db);

        db.start_session(p.id, "first", "manual").unwrap();
        let err = db.start_session(p.id, "second", "manual").unwrap_err();
        assert!(err.to_string().contains("already active"), "{}", err);
    }

    /// Stopping a session snapshots the events that landed inside its time
    /// window into session_events, so later recovery can use stable membership.
    #[test]
    fn session_stop_links_events_in_window() {
        let db = db();
        let p = project(&db);
        let session = db.start_session(p.id, "agent-run", "manual").unwrap();
        db.insert_event(
            p.id,
            "/home/user/project/src/auth.rs",
            "MODIFIED",
            Some("auth_hash"),
            Some("old_auth_hash"),
            None,
            None,
            Some(10),
        )
        .unwrap();

        let stopped = db.stop_active_session(p.id).unwrap().unwrap();
        assert_eq!(stopped.id, session.id);

        let events = db.get_session_events(&stopped).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "/home/user/project/src/auth.rs");
    }

    /// Event-id anchors keep same-second baseline events out of the session. A
    /// timestamp-only window would include both rows because all calls can land
    /// inside one second.
    #[test]
    fn session_events_exclude_same_second_events_before_start() {
        let db = db();
        let p = project(&db);
        db.insert_event(
            p.id,
            "/home/user/project/src/baseline.rs",
            "MODIFIED",
            Some("baseline_hash"),
            None,
            None,
            None,
            Some(10),
        )
        .unwrap();
        let session = db.start_session(p.id, "agent-run", "manual").unwrap();
        assert!(session.start_event_id > 0);
        db.insert_event(
            p.id,
            "/home/user/project/src/agent.rs",
            "MODIFIED",
            Some("agent_hash"),
            Some("baseline_hash"),
            None,
            None,
            Some(10),
        )
        .unwrap();

        let stopped = db.stop_active_session(p.id).unwrap().unwrap();
        let paths = db
            .get_session_events(&stopped)
            .unwrap()
            .into_iter()
            .map(|event| event.path)
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["/home/user/project/src/agent.rs"]);
    }

    // ── file_state ───────────────────────────────────────────────────

    /// File state written with upsert must be readable back with the correct
    /// hash, exists_now flag, and the size/mtime used by the #26 fast path.
    #[test]
    fn upsert_and_retrieve_file_state() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/main.rs";
        db.upsert_file_state(p.id, path, "deadbeef", true, 1234, Some(987_654_321))
            .unwrap();
        let state = db.get_file_state(p.id, path).unwrap().unwrap();
        assert_eq!(state.latest_hash, Some("deadbeef".to_string()));
        assert!(state.exists_now);
        assert_eq!(state.size, Some(1234));
        assert_eq!(state.mtime_nanos, Some(987_654_321));
    }

    /// A database created before #26 lacks the `size`/`mtime_nanos` columns.
    /// `apply_schema` must add them in place (additive migration) without losing
    /// existing rows, and legacy rows must read back as NULL size/mtime so the
    /// fast path is simply disabled for them rather than misbehaving.
    #[test]
    fn migration_backfills_size_and_mtime_columns_on_legacy_db() {
        let conn = Connection::open_in_memory().unwrap();
        // Pre-#26 file_state schema: no size / mtime_nanos.
        conn.execute_batch(
            "CREATE TABLE file_state (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id   INTEGER NOT NULL,
                path         TEXT    NOT NULL,
                latest_hash  TEXT,
                last_seen_at INTEGER NOT NULL,
                exists_now   INTEGER NOT NULL DEFAULT 1,
                UNIQUE(project_id, path)
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_state (project_id, path, latest_hash, last_seen_at, exists_now)
             VALUES (1, '/legacy.rs', 'oldhash', 0, 1)",
            [],
        )
        .unwrap();

        // Running the schema migration must add the columns, not drop the row.
        apply_schema(&conn).unwrap();

        let db = Database { conn };
        let state = db.get_file_state(1, "/legacy.rs").unwrap().unwrap();
        assert_eq!(state.latest_hash.as_deref(), Some("oldhash"));
        assert_eq!(state.size, None, "legacy rows have no recorded size");
        assert_eq!(
            state.mtime_nanos, None,
            "legacy rows have no recorded mtime"
        );

        // And the migrated table accepts new size/mtime writes.
        db.upsert_file_state(1, "/legacy.rs", "newhash", true, 42, Some(7))
            .unwrap();
        let updated = db.get_file_state(1, "/legacy.rs").unwrap().unwrap();
        assert_eq!(updated.size, Some(42));
        assert_eq!(updated.mtime_nanos, Some(7));
    }

    // ── retention methods ───────────────────────────────────────────

    fn seed_events(db: &Database, project_id: i64) {
        let now = chrono::Utc::now().timestamp();
        // Old event: 10 days ago
        db.conn
            .execute(
                "INSERT INTO file_events (project_id, timestamp, path, event_type, current_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project_id,
                    now - 864_000,
                    "/p/old.rs",
                    "MODIFIED",
                    "hash_old"
                ],
            )
            .unwrap();
        // Recent event: 1 hour ago
        db.conn
            .execute(
                "INSERT INTO file_events (project_id, timestamp, path, event_type, current_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                params![project_id, now - 3600, "/p/new.rs", "MODIFIED", "hash_new"],
            )
            .unwrap();
    }

    /// Only events older than the cutoff timestamp are included in the count.
    #[test]
    fn count_events_before_counts_old_events() {
        let db = db();
        let p = project(&db);
        seed_events(&db, p.id);
        let now = chrono::Utc::now().timestamp();
        let cutoff = now - 86400; // 1 day ago
        assert_eq!(db.count_events_before(p.id, cutoff).unwrap(), 1);
    }

    /// Events older than the cutoff are deleted; newer events are kept.
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

    /// All hashes referenced by current events are returned as live.
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

    /// After pruning old events, their hashes are no longer live.
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

    /// A hash referenced only by file_state (the file's current on-disk content,
    /// whose creating event has already been pruned) must still be considered live —
    /// otherwise retention would orphan the only snapshot of an existing file.
    #[test]
    fn get_live_hashes_includes_file_state_latest_hash() {
        let db = db();
        let p = project(&db);
        // No events exist for this path; only file_state references the hash.
        db.upsert_file_state(p.id, "/p/extant.rs", "fs_only_hash", true, 0, None)
            .unwrap();
        let hashes = db.get_live_hashes(p.id).unwrap();
        assert!(
            hashes.contains("fs_only_hash"),
            "file_state.latest_hash must be considered live: {:?}",
            hashes
        );
    }

    /// A file_state row whose `exists_now = 0` must NOT pin its `latest_hash`
    /// as live. `mark_deleted` only flips `exists_now` and leaves
    /// `latest_hash` intact, so without the `exists_now = 1` filter every
    /// file the user ever deletes anchors its snapshot forever and retention
    /// can never reclaim that disk space — defeating the size cap.
    ///
    /// This also separately verifies the orphan path: once the only event
    /// referencing the hash is pruned and the file is marked deleted,
    /// nothing pins the snapshot.
    #[test]
    fn get_live_hashes_excludes_deleted_file_state_rows() {
        let db = db();
        let p = project(&db);
        let path = "/p/old_and_gone.rs";

        // Track the file, then mark it deleted (mimicking `handle_delete`,
        // which leaves `latest_hash` populated).
        db.upsert_file_state(p.id, path, "ghost_hash", true, 0, None)
            .unwrap();
        db.mark_deleted(p.id, path).unwrap();
        let state = db.get_file_state(p.id, path).unwrap().unwrap();
        assert!(!state.exists_now, "test setup: file must be marked deleted");
        assert_eq!(
            state.latest_hash.as_deref(),
            Some("ghost_hash"),
            "test setup: latest_hash must persist past mark_deleted — that's the bug surface"
        );

        // No events reference ghost_hash. With the bug, the file_state arm of
        // the UNION still reports it as live, leaking the snapshot.
        let hashes = db.get_live_hashes(p.id).unwrap();
        assert!(
            !hashes.contains("ghost_hash"),
            "deleted file's latest_hash must not be live: {:?}",
            hashes
        );
    }

    /// A surviving DELETED event must keep its `previous_hash` (the file's last
    /// captured content) alive, so a just-deleted file whose creating event has
    /// aged out of retention is still recoverable. Without this pin the only
    /// snapshot of the deleted file is orphaned and pruned immediately.
    /// (Red before adding the DELETED `previous_hash` arm to `get_live_hashes`.)
    #[test]
    fn get_live_hashes_pins_deleted_event_previous_hash() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/gone.rs";

        // Simulate handle_delete: a DELETED event whose previous_hash is the
        // last known content, and no surviving non-DELETE event for the path.
        db.insert_event(
            p.id,
            path,
            "DELETED",
            None,
            Some("last_content_hash"),
            None,
            None,
            None,
        )
        .unwrap();

        let hashes = db.get_live_hashes(p.id).unwrap();
        assert!(
            hashes.contains("last_content_hash"),
            "a deleted file's last content must stay live while its DELETED \
             event survives: {:?}",
            hashes
        );
    }

    /// A surviving MODIFIED event's `previous_hash` can be the only reference to
    /// content overwritten by a rename-overwrite event sequence on Linux. Keep it
    /// live while the event itself remains inside the retention window.
    #[test]
    fn get_live_hashes_pins_modified_event_previous_hash() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/modified.rs";

        db.insert_event(
            p.id,
            path,
            "MODIFIED",
            Some("new_content_hash"),
            Some("old_content_hash"),
            None,
            None,
            None,
        )
        .unwrap();

        let hashes = db.get_live_hashes(p.id).unwrap();
        assert!(
            hashes.contains("new_content_hash"),
            "current_hash from MODIFIED event must stay live: {:?}",
            hashes
        );
        assert!(
            hashes.contains("old_content_hash"),
            "previous_hash from surviving MODIFIED event must stay live: {:?}",
            hashes
        );
    }

    /// Once the event carrying a `previous_hash` ages out, that hash is no
    /// longer pinned just because it used to be previous content.
    #[test]
    fn get_live_hashes_drops_previous_hash_after_event_prune() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/modified.rs";

        db.insert_event(
            p.id,
            path,
            "MODIFIED",
            Some("new_content_hash"),
            Some("old_content_hash"),
            None,
            None,
            None,
        )
        .unwrap();
        db.delete_events_before(p.id, i64::MAX).unwrap();

        let hashes = db.get_live_hashes(p.id).unwrap();
        assert!(
            !hashes.contains("old_content_hash"),
            "previous_hash must not remain live after its event is pruned: {:?}",
            hashes
        );
    }

    /// Returns the most recent DELETED event so restore can fall back to its
    /// previous_hash; returns None when the file was never deleted.
    #[test]
    fn get_latest_deleted_event_finds_deletion() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/gone.rs";

        assert!(
            db.get_latest_deleted_event(p.id, path).unwrap().is_none(),
            "no deletion recorded yet"
        );

        db.insert_event(
            p.id,
            path,
            "DELETED",
            None,
            Some("prev_hash"),
            None,
            None,
            None,
        )
        .unwrap();

        let ev = db.get_latest_deleted_event(p.id, path).unwrap().unwrap();
        assert_eq!(ev.event_type, "DELETED");
        assert_eq!(ev.previous_hash.as_deref(), Some("prev_hash"));
    }

    /// All created project IDs appear in the returned list.
    #[test]
    fn get_all_project_ids_returns_existing_projects() {
        let db = db();
        let p1 = db.get_or_create_project(Path::new("/a")).unwrap();
        let p2 = db.get_or_create_project(Path::new("/b")).unwrap();
        let ids = db.get_all_project_ids().unwrap();
        assert!(ids.contains(&p1.id));
        assert!(ids.contains(&p2.id));
    }

    /// Returns the most recently timestamped event for the given path.
    #[test]
    fn get_latest_event_returns_most_recent() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/foo.rs";
        // Insert two events for the same path via insert_event_at so we control ordering.
        let now = chrono::Utc::now().timestamp();
        db.insert_event_at(p.id, path, "CREATED", now - 100)
            .unwrap();
        db.insert_event_at(p.id, path, "MODIFIED", now - 10)
            .unwrap();
        let event = db.get_latest_event(p.id, path).unwrap().unwrap();
        assert_eq!(event.event_type, "MODIFIED");
    }

    /// Events inserted in one transaction can share the same second-precision
    /// timestamp. Tie-break by row id so the later insert is the latest event.
    #[test]
    fn get_latest_event_tie_breaks_same_timestamp_by_insert_order() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/renamed_over.rs";

        db.insert_event(
            p.id,
            path,
            "DELETED",
            None,
            Some("overwritten_hash"),
            None,
            None,
            None,
        )
        .unwrap();
        db.insert_event(
            p.id,
            path,
            "RENAMED",
            Some("new_hash"),
            Some("source_hash"),
            None,
            Some("/home/user/project/source.rs"),
            None,
        )
        .unwrap();

        let event = db.get_latest_event(p.id, path).unwrap().unwrap();
        assert_eq!(event.event_type, "RENAMED");
        assert_eq!(event.current_hash.as_deref(), Some("new_hash"));
    }

    /// DELETED events are never returned as restorable; only non-delete events at or before the
    /// given timestamp are considered.
    #[test]
    fn get_event_at_time_excludes_deleted_and_respects_cutoff() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/bar.rs";
        let now = chrono::Utc::now().timestamp();
        // Seed: CREATED long ago, MODIFIED in the middle, DELETED recently.
        db.insert_event_at(p.id, path, "CREATED", now - 300)
            .unwrap();
        db.insert_event_at(p.id, path, "MODIFIED", now - 200)
            .unwrap();
        db.insert_event_at(p.id, path, "DELETED", now - 100)
            .unwrap();

        // Querying at now-150 should return MODIFIED (newest non-DELETE at or before that point).
        let event = db
            .get_event_at_time(p.id, path, now - 150)
            .unwrap()
            .unwrap();
        assert_eq!(event.event_type, "MODIFIED");

        // Querying before any event returns None.
        let none = db.get_event_at_time(p.id, path, now - 1000).unwrap();
        assert!(none.is_none());
    }

    /// After mark_deleted the file's exists_now flag is false, signalling it is gone from disk.
    #[test]
    fn mark_deleted_sets_exists_now_false() {
        let db = db();
        let p = project(&db);
        let path = "/home/user/project/gone.rs";
        db.upsert_file_state(p.id, path, "abc123", true, 0, None)
            .unwrap();
        // Confirm it's alive before we delete it.
        assert!(db.get_file_state(p.id, path).unwrap().unwrap().exists_now);
        db.mark_deleted(p.id, path).unwrap();
        let state = db.get_file_state(p.id, path).unwrap().unwrap();
        assert!(!state.exists_now);
    }
}
