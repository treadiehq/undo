use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::models::{
    Checkpoint, FileEvent, FileState, Recovery, RecoveryEntry, RunBoundary, RunIntent, Session,
    WatchedProject,
};

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
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    )?;

    let schema_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if schema_version > 4 {
        anyhow::bail!(
            "Undo data uses schema version {}, but this binary supports through version 4. \
             Upgrade Undo before opening this history.",
            schema_version
        );
    }

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
            run_id      INTEGER,
            name       TEXT    NOT NULL,
            timestamp  INTEGER NOT NULL,
            event_id    INTEGER,
            intent      TEXT,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            FOREIGN KEY (run_id) REFERENCES sessions(id)
        );

        CREATE TABLE IF NOT EXISTS sessions (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id INTEGER NOT NULL,
            name       TEXT    NOT NULL,
            kind       TEXT    NOT NULL,
            attribution_mode TEXT NOT NULL DEFAULT 'window',
            actor      TEXT    NOT NULL DEFAULT 'human',
            agent      TEXT,
            command    TEXT,
            intent     TEXT,
            external_id TEXT,
            status     TEXT    NOT NULL DEFAULT 'active',
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

        CREATE TABLE IF NOT EXISTS run_boundaries (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id             INTEGER NOT NULL,
            external_change_id TEXT    NOT NULL,
            status             TEXT    NOT NULL DEFAULT 'open',
            start_event_id     INTEGER NOT NULL,
            end_event_id       INTEGER,
            started_at         INTEGER NOT NULL,
            ended_at           INTEGER,
            created_at         INTEGER NOT NULL,
            updated_at         INTEGER NOT NULL,
            FOREIGN KEY (run_id) REFERENCES sessions(id) ON DELETE CASCADE,
            UNIQUE(run_id, external_change_id)
        );

        CREATE TABLE IF NOT EXISTS run_boundary_paths (
            boundary_id INTEGER NOT NULL,
            path        TEXT    NOT NULL,
            PRIMARY KEY (boundary_id, path),
            FOREIGN KEY (boundary_id) REFERENCES run_boundaries(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS run_intents (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id         INTEGER NOT NULL,
            label          TEXT    NOT NULL,
            status         TEXT    NOT NULL DEFAULT 'active',
            start_event_id INTEGER NOT NULL,
            end_event_id   INTEGER,
            started_at     INTEGER NOT NULL,
            ended_at       INTEGER,
            FOREIGN KEY (run_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS recoveries (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id  INTEGER NOT NULL,
            run_id      INTEGER,
            request     TEXT    NOT NULL,
            kind        TEXT    NOT NULL,
            status      TEXT    NOT NULL DEFAULT 'planned',
            confidence  TEXT    NOT NULL DEFAULT 'exact',
            ambiguity   TEXT,
            created_at  INTEGER NOT NULL,
            expires_at  INTEGER NOT NULL,
            applied_at  INTEGER,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            FOREIGN KEY (run_id) REFERENCES sessions(id)
        );

        CREATE TABLE IF NOT EXISTS recovery_entries (
            recovery_id    INTEGER NOT NULL,
            path           TEXT    NOT NULL,
            action         TEXT    NOT NULL,
            target_hash    TEXT,
            source_timestamp INTEGER,
            expected_hash  TEXT,
            expected_exists INTEGER NOT NULL,
            PRIMARY KEY (recovery_id, path),
            FOREIGN KEY (recovery_id) REFERENCES recoveries(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS integration_events (
            idempotency_key TEXT PRIMARY KEY,
            run_id          INTEGER,
            event_type      TEXT NOT NULL,
            request_hash    TEXT NOT NULL DEFAULT '',
            response_json   TEXT NOT NULL,
            created_at      INTEGER NOT NULL,
            FOREIGN KEY (run_id) REFERENCES sessions(id)
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
        CREATE INDEX IF NOT EXISTS idx_session_events_event
            ON session_events(event_id);
        CREATE INDEX IF NOT EXISTS idx_run_boundaries_run
            ON run_boundaries(run_id, start_event_id);
        CREATE INDEX IF NOT EXISTS idx_run_boundary_paths_path
            ON run_boundary_paths(path);
        CREATE INDEX IF NOT EXISTS idx_run_intents_run
            ON run_intents(run_id, start_event_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_run_intents_one_active
            ON run_intents(run_id) WHERE ended_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_recoveries_project
            ON recoveries(project_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_recovery_entries_target
            ON recovery_entries(target_hash);",
    )?;

    add_column_if_missing(conn, "file_state", "size", "INTEGER")?;
    add_column_if_missing(conn, "file_state", "mtime_nanos", "INTEGER")?;
    add_column_if_missing(
        conn,
        "sessions",
        "start_event_id",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "sessions", "end_event_id", "INTEGER")?;
    add_column_if_missing(conn, "sessions", "actor", "TEXT NOT NULL DEFAULT 'human'")?;
    add_column_if_missing(conn, "sessions", "agent", "TEXT")?;
    add_column_if_missing(conn, "sessions", "command", "TEXT")?;
    add_column_if_missing(conn, "sessions", "intent", "TEXT")?;
    add_column_if_missing(conn, "sessions", "external_id", "TEXT")?;
    add_column_if_missing(conn, "sessions", "status", "TEXT NOT NULL DEFAULT 'active'")?;
    add_column_if_missing(
        conn,
        "sessions",
        "attribution_mode",
        "TEXT NOT NULL DEFAULT 'window'",
    )?;
    add_column_if_missing(
        conn,
        "integration_events",
        "request_hash",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(conn, "checkpoints", "run_id", "INTEGER")?;
    add_column_if_missing(conn, "checkpoints", "event_id", "INTEGER")?;
    add_column_if_missing(conn, "checkpoints", "intent", "TEXT")?;
    migrate_checkpoints_v3(conn)?;
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_sessions_one_active;
         CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_external_id
            ON sessions(project_id, external_id) WHERE external_id IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_legacy_name
            ON checkpoints(project_id, name) WHERE run_id IS NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_run_name
            ON checkpoints(run_id, name) WHERE run_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_checkpoints_project_time
            ON checkpoints(project_id, timestamp);",
    )?;

    conn.execute(
        "UPDATE sessions
         SET status = CASE WHEN ended_at IS NULL THEN 'active' ELSE 'completed' END
         WHERE status IS NULL OR status = '' OR (status = 'active' AND ended_at IS NOT NULL)",
        [],
    )?;
    conn.pragma_update(None, "user_version", 4)?;

    Ok(())
}

fn migrate_checkpoints_v3(conn: &Connection) -> Result<()> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'checkpoints'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let requires_rebuild = sql
        .as_deref()
        .is_some_and(|sql| sql.contains("UNIQUE(project_id, name)"));
    if !requires_rebuild {
        return Ok(());
    }

    conn.execute_batch(
        "PRAGMA foreign_keys=OFF;
         BEGIN IMMEDIATE;
         CREATE TABLE checkpoints_v3 (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id INTEGER NOT NULL,
            run_id      INTEGER,
            name       TEXT    NOT NULL,
            timestamp  INTEGER NOT NULL,
            event_id    INTEGER,
            intent      TEXT,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (project_id) REFERENCES watched_projects(id),
            FOREIGN KEY (run_id) REFERENCES sessions(id)
         );
         INSERT INTO checkpoints_v3
            (id, project_id, run_id, name, timestamp, event_id, intent, created_at)
         SELECT id, project_id, run_id, name, timestamp, event_id, intent, created_at
         FROM checkpoints;
         DROP TABLE checkpoints;
         ALTER TABLE checkpoints_v3 RENAME TO checkpoints;
         COMMIT;
         PRAGMA foreign_keys=ON;",
    )?;
    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
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
        self.transaction_with("BEGIN", f)
    }

    /// Run `f` after acquiring SQLite's write-reservation lock. Use this for
    /// event boundaries: the lock makes the boundary's timestamp, maximum event
    /// ID, and persisted row one atomic point relative to daemon event commits.
    pub fn immediate_transaction<R>(&self, f: impl FnOnce(&Self) -> Result<R>) -> Result<R> {
        self.transaction_with("BEGIN IMMEDIATE", f)
    }

    fn transaction_with<R>(
        &self,
        begin_statement: &str,
        f: impl FnOnce(&Self) -> Result<R>,
    ) -> Result<R> {
        #[cfg(debug_assertions)]
        IN_TRANSACTION.with(|flag| {
            assert!(
                !flag.get(),
                "database transactions are not reentrant — a nested call would emit BEGIN within BEGIN"
            );
            flag.set(true);
        });

        let result = (|| {
            self.conn.execute_batch(begin_statement)?;
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

    pub fn get_project_by_id(&self, project_id: i64) -> Result<Option<WatchedProject>> {
        self.conn
            .query_row(
                "SELECT id, root_path, created_at
                 FROM watched_projects WHERE id = ?1",
                params![project_id],
                |row| {
                    Ok(WatchedProject {
                        id: row.get(0)?,
                        root_path: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .context("failed to query project by id")
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

    pub fn get_events_since_limited(
        &self,
        project_id: i64,
        since_timestamp: i64,
        limit: usize,
    ) -> Result<Vec<FileEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, timestamp, path, event_type,
                    current_hash, previous_hash, snapshot_path, old_path, file_size
             FROM file_events
             WHERE project_id = ?1 AND timestamp >= ?2
             ORDER BY timestamp DESC, id DESC
             LIMIT ?3",
        )?;
        let events = stmt.query_map(
            params![project_id, since_timestamp, limit as i64],
            row_to_event,
        )?;
        events
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query limited events")
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

    /// Return the latest event that determines whether `path` existed at a
    /// point in time. Unlike `get_event_at_time`, deletion and rename-away
    /// events are included because absence is a real recoverable state.
    pub fn get_path_state_event_at_time(
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
                   AND (path = ?2 OR old_path = ?2)
                   AND timestamp <= ?3
                 ORDER BY timestamp DESC, id DESC
                 LIMIT 1",
                params![project_id, path, before_ts],
                row_to_event,
            )
            .optional()
            .context("failed to query path state at time")
    }

    pub fn get_path_state_event_at_id(
        &self,
        project_id: i64,
        path: &str,
        event_id: i64,
    ) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1
                   AND (path = ?2 OR old_path = ?2)
                   AND id <= ?3
                 ORDER BY id DESC
                 LIMIT 1",
                params![project_id, path, event_id],
                row_to_event,
            )
            .optional()
            .context("failed to query path state at event boundary")
    }

    /// Find the first event after a restore target that changed whether `path`
    /// existed or what it contained. Matching `old_path` makes renames usable
    /// for reconstructing the source name after older history is pruned.
    pub fn get_first_path_event_after(
        &self,
        project_id: i64,
        path: &str,
        after_ts: i64,
    ) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1
                   AND timestamp > ?3
                   AND (path = ?2 OR old_path = ?2)
                 ORDER BY timestamp ASC, id ASC
                 LIMIT 1",
                params![project_id, path, after_ts],
                row_to_event,
            )
            .optional()
            .context("failed to query first path event after target")
    }

    pub fn get_first_path_event_after_id(
        &self,
        project_id: i64,
        path: &str,
        event_id: i64,
    ) -> Result<Option<FileEvent>> {
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1
                   AND id > ?3
                   AND (path = ?2 OR old_path = ?2)
                 ORDER BY id ASC
                 LIMIT 1",
                params![project_id, path, event_id],
                row_to_event,
            )
            .optional()
            .context("failed to query first path event after boundary")
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

    /// Find the first event involving `path` inside a session. Event-id fences
    /// preserve the exact start boundary even when several events share a
    /// second, and `old_path` exposes the source side of renames.
    pub fn get_first_path_event_in_session(
        &self,
        session: &Session,
        path: &str,
    ) -> Result<Option<FileEvent>> {
        let end_event_id = match session.end_event_id {
            Some(id) => id,
            None => self.max_event_id(session.project_id)?,
        };
        self.conn
            .query_row(
                "SELECT id, project_id, timestamp, path, event_type,
                        current_hash, previous_hash, snapshot_path, old_path, file_size
                 FROM file_events
                 WHERE project_id = ?1
                   AND id > ?3
                   AND id <= ?4
                   AND (path = ?2 OR old_path = ?2)
                 ORDER BY id ASC
                 LIMIT 1",
                params![
                    session.project_id,
                    path,
                    session.start_event_id,
                    end_event_id
                ],
                row_to_event,
            )
            .optional()
            .context("failed to query first path event in session")
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

    pub fn event_time_bounds(&self, project_id: i64) -> Result<(Option<i64>, Option<i64>)> {
        self.conn
            .query_row(
                "SELECT MIN(timestamp), MAX(timestamp)
                 FROM file_events WHERE project_id = ?1",
                params![project_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("failed to query event time bounds")
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
             WHERE project_id = ?1 AND previous_hash IS NOT NULL
             UNION
             SELECT re.target_hash
             FROM recovery_entries re
             JOIN recoveries r ON r.id = re.recovery_id
             WHERE r.project_id = ?1
               AND r.status = 'planned'
               AND r.expires_at >= unixepoch()
               AND re.target_hash IS NOT NULL",
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
        self.start_run(project_id, name, kind, "human", None, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_run(
        &self,
        project_id: i64,
        name: &str,
        kind: &str,
        actor: &str,
        agent: Option<&str>,
        command: Option<&str>,
        intent: Option<&str>,
        external_id: Option<&str>,
    ) -> Result<Session> {
        self.start_run_with_attribution(
            project_id,
            name,
            kind,
            "window",
            actor,
            agent,
            command,
            intent,
            external_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_reported_run(
        &self,
        project_id: i64,
        name: &str,
        kind: &str,
        actor: &str,
        agent: Option<&str>,
        command: Option<&str>,
        intent: Option<&str>,
        external_id: &str,
    ) -> Result<Session> {
        self.start_run_with_attribution(
            project_id,
            name,
            kind,
            "reported",
            actor,
            agent,
            command,
            intent,
            Some(external_id),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_run_with_attribution(
        &self,
        project_id: i64,
        name: &str,
        kind: &str,
        attribution_mode: &str,
        actor: &str,
        agent: Option<&str>,
        command: Option<&str>,
        intent: Option<&str>,
        external_id: Option<&str>,
    ) -> Result<Session> {
        self.immediate_transaction(|db| {
            let now = Utc::now().timestamp();
            if let Some(external_id) = external_id
                && let Some(existing) = db.get_run_by_external_id(project_id, external_id)?
            {
                if existing.attribution_mode != attribution_mode {
                    anyhow::bail!(
                        "external Run ID '{}' already belongs to a {} Run",
                        external_id,
                        existing.attribution_mode
                    );
                }
                return Ok(existing);
            }
            let active_runs = db.list_active_runs(project_id)?;
            let conflict = if attribution_mode == "window" {
                active_runs.first()
            } else {
                active_runs
                    .iter()
                    .find(|run| run.attribution_mode == "window")
            };
            if let Some(active) = conflict {
                anyhow::bail!(
                    "Run {} ('{}', {} attribution) is already active. Complete it before starting this {} Run.",
                    active.public_id(),
                    active.name,
                    active.attribution_mode,
                    attribution_mode,
                );
            }
            let start_event_id = db.max_event_id(project_id)?;
            db.conn.execute(
                "INSERT INTO sessions
                    (project_id, name, kind, attribution_mode, actor, agent, command, intent,
                     external_id, status, started_at, start_event_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', ?10, ?11, ?12)",
                params![
                    project_id,
                    name,
                    kind,
                    attribution_mode,
                    actor,
                    agent,
                    command,
                    intent,
                    external_id,
                    now,
                    start_event_id,
                    now
                ],
            )?;
            db.get_session_by_id(db.conn.last_insert_rowid())?
                .ok_or_else(|| anyhow::anyhow!("failed to read Run after creating it"))
        })
    }

    pub fn stop_active_session(&self, project_id: i64) -> Result<Option<Session>> {
        let Some(session) = self.get_active_session(project_id)? else {
            return Ok(None);
        };
        self.complete_run(session.id, "completed").map(Some)
    }

    pub fn complete_run(&self, run_id: i64, status: &str) -> Result<Session> {
        self.immediate_transaction(|db| {
            let session = db.get_session_by_id(run_id)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Run r_{} not found. Use `undo run list` to see available Runs.",
                    run_id
                )
            })?;
            if session.ended_at.is_some() {
                return Ok(session);
            }
            let ended_at = Utc::now().timestamp();
            let end_event_id = db.max_event_id(session.project_id)?;
            if let Some(intent) = db.get_active_run_intent(session.id)? {
                db.complete_run_intent_at(session.id, Some(&intent.label), end_event_id, ended_at)?;
            }
            if session.is_reported() {
                db.abort_open_boundaries_at(session.id, ended_at)?;
            }
            db.conn.execute(
                "UPDATE sessions
                 SET ended_at = ?1, end_event_id = ?2, status = ?3
                 WHERE id = ?4",
                params![ended_at, end_event_id, status, session.id],
            )?;
            if !session.is_reported() {
                db.link_session_events(
                    session.id,
                    session.project_id,
                    session.started_at,
                    ended_at,
                    session.start_event_id,
                    end_event_id,
                )?;
            }
            db.get_session_by_id(session.id)?
                .ok_or_else(|| anyhow::anyhow!("failed to read completed Run"))
        })
    }

    pub fn list_sessions(&self, project_id: i64) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, kind, attribution_mode, actor, agent, command, intent,
                    external_id, status, started_at, ended_at,
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
                "SELECT id, project_id, name, kind, attribution_mode, actor, agent, command, intent,
                        external_id, status, started_at, ended_at,
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
        let active = self.list_active_runs(project_id)?;
        match active.as_slice() {
            [] => Ok(None),
            [run] => Ok(Some(run.clone())),
            runs => {
                let ids = runs
                    .iter()
                    .map(Session::public_id)
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "multiple Runs are active ({ids}); specify a Run explicitly with --run <RUN> or run_id"
                )
            }
        }
    }

    pub fn list_active_runs(&self, project_id: i64) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, kind, attribution_mode, actor, agent, command, intent,
                    external_id, status, started_at, ended_at,
                    start_event_id, end_event_id, created_at
             FROM sessions
             WHERE project_id = ?1 AND ended_at IS NULL
             ORDER BY started_at DESC, id DESC",
        )?;
        let runs = stmt.query_map(params![project_id], row_to_session)?;
        runs.collect::<Result<Vec<_>, _>>()
            .context("failed to query active Runs")
    }

    pub fn get_session_events(&self, session: &Session) -> Result<Vec<FileEvent>> {
        let mapped_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session_events WHERE session_id = ?1",
            params![session.id],
            |row| row.get(0),
        )?;

        if mapped_count > 0 || session.is_reported() {
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

    pub fn get_session_by_id(&self, session_id: i64) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, kind, attribution_mode, actor, agent, command, intent,
                        external_id, status, started_at, ended_at,
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

    pub fn get_run_by_ref(&self, project_id: i64, reference: &str) -> Result<Option<Session>> {
        if let Some(id) = parse_public_id(reference, "r_") {
            return self
                .get_session_by_id(id)
                .map(|run| run.filter(|run| run.project_id == project_id));
        }
        self.get_session_by_name(project_id, reference)
    }

    pub fn get_run_by_external_id(
        &self,
        project_id: i64,
        external_id: &str,
    ) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, kind, attribution_mode, actor, agent, command, intent,
                        external_id, status, started_at, ended_at,
                        start_event_id, end_event_id, created_at
                 FROM sessions
                 WHERE project_id = ?1 AND external_id = ?2
                 LIMIT 1",
                params![project_id, external_id],
                row_to_session,
            )
            .optional()
            .context("failed to query Run by external id")
    }

    // ── Reported change boundaries and ownership claims ─────────────

    /// Open an exact-path reported boundary at the current event fence.
    /// These rows record integration claims; they are not forensic evidence
    /// that a particular process authored a filesystem event.
    pub fn open_run_boundary(
        &self,
        run_id: i64,
        external_change_id: &str,
        paths: &[String],
    ) -> Result<RunBoundary> {
        self.immediate_transaction(|db| {
            let run = db
                .get_session_by_id(run_id)?
                .ok_or_else(|| anyhow::anyhow!("Run r_{run_id} not found"))?;
            if !run.is_reported() {
                anyhow::bail!(
                    "Run {} uses window attribution and cannot accept explicit change claims",
                    run.public_id()
                );
            }
            if !run.is_active() {
                anyhow::bail!("Run {} is already complete", run.public_id());
            }
            let requested = db.validated_boundary_paths(&run, paths)?;
            if let Some(existing) =
                db.get_run_boundary_by_external_id(run_id, external_change_id)?
            {
                db.ensure_boundary_paths(existing.id, &requested)?;
                return Ok(existing);
            }

            let now = Utc::now().timestamp();
            let start_event_id = db.max_event_id(run.project_id)?;
            db.conn.execute(
                "INSERT INTO run_boundaries
                    (run_id, external_change_id, status, start_event_id,
                     started_at, created_at, updated_at)
                 VALUES (?1, ?2, 'open', ?3, ?4, ?4, ?4)",
                params![run.id, external_change_id, start_event_id, now],
            )?;
            let boundary_id = db.conn.last_insert_rowid();
            for path in requested {
                db.conn.execute(
                    "INSERT INTO run_boundary_paths (boundary_id, path) VALUES (?1, ?2)",
                    params![boundary_id, path],
                )?;
            }
            db.get_run_boundary_by_id(boundary_id)?
                .ok_or_else(|| anyhow::anyhow!("failed to read reported change boundary"))
        })
    }

    /// Close a boundary at the current event fence and claim only events whose
    /// `path` or rename `old_path` exactly matches the opening path set.
    pub fn close_run_boundary(
        &self,
        run_id: i64,
        external_change_id: &str,
        paths: &[String],
    ) -> Result<RunBoundary> {
        self.immediate_transaction(|db| {
            let run = db
                .get_session_by_id(run_id)?
                .ok_or_else(|| anyhow::anyhow!("Run r_{run_id} not found"))?;
            let boundary = db
                .get_run_boundary_by_external_id(run_id, external_change_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "change '{}' was not opened for Run {}",
                        external_change_id,
                        run.public_id()
                    )
                })?;
            let requested = db.validated_boundary_paths(&run, paths)?;
            db.ensure_boundary_paths(boundary.id, &requested)?;
            if boundary.status == "closed" {
                return Ok(boundary);
            }
            if boundary.status == "aborted" {
                anyhow::bail!(
                    "change '{}' was aborted when Run {} completed",
                    external_change_id,
                    run.public_id()
                );
            }
            if !run.is_active() {
                anyhow::bail!("Run {} is already complete", run.public_id());
            }

            let now = Utc::now().timestamp();
            let end_event_id = db.max_event_id(run.project_id)?;
            db.conn.execute(
                "UPDATE run_boundaries
                 SET status = 'closed', end_event_id = ?1, ended_at = ?2, updated_at = ?2
                 WHERE id = ?3 AND status = 'open'",
                params![end_event_id, now, boundary.id],
            )?;
            db.conn.execute(
                "INSERT OR IGNORE INTO session_events (session_id, event_id)
                 SELECT ?1, e.id
                 FROM file_events e
                 JOIN run_boundary_paths bp
                   ON bp.boundary_id = ?2
                  AND (e.path = bp.path OR e.old_path = bp.path)
                 WHERE e.project_id = ?3
                   AND e.id > ?4
                   AND e.id <= ?5",
                params![
                    run.id,
                    boundary.id,
                    run.project_id,
                    boundary.start_event_id,
                    end_event_id
                ],
            )?;
            db.get_run_boundary_by_id(boundary.id)?
                .ok_or_else(|| anyhow::anyhow!("failed to read closed change boundary"))
        })
    }

    pub fn abort_run_boundary(&self, run_id: i64, external_change_id: &str) -> Result<RunBoundary> {
        self.immediate_transaction(|db| {
            let boundary = db
                .get_run_boundary_by_external_id(run_id, external_change_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "change '{}' was not opened for Run r_{}",
                        external_change_id,
                        run_id
                    )
                })?;
            if boundary.status == "open" {
                db.conn.execute(
                    "UPDATE run_boundaries
                     SET status = 'aborted', ended_at = ?1, updated_at = ?1
                     WHERE id = ?2",
                    params![Utc::now().timestamp(), boundary.id],
                )?;
            }
            db.get_run_boundary_by_id(boundary.id)?
                .ok_or_else(|| anyhow::anyhow!("failed to read aborted change boundary"))
        })
    }

    fn abort_open_boundaries_at(&self, run_id: i64, ended_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE run_boundaries
             SET status = 'aborted', ended_at = ?1, updated_at = ?1
             WHERE run_id = ?2 AND status = 'open'",
            params![ended_at, run_id],
        )?;
        Ok(())
    }

    pub fn get_run_boundary_by_external_id(
        &self,
        run_id: i64,
        external_change_id: &str,
    ) -> Result<Option<RunBoundary>> {
        self.conn
            .query_row(
                "SELECT id, run_id, external_change_id, status, start_event_id,
                        end_event_id, started_at, ended_at, created_at, updated_at
                 FROM run_boundaries
                 WHERE run_id = ?1 AND external_change_id = ?2",
                params![run_id, external_change_id],
                row_to_run_boundary,
            )
            .optional()
            .context("failed to query reported change boundary")
    }

    fn get_run_boundary_by_id(&self, boundary_id: i64) -> Result<Option<RunBoundary>> {
        self.conn
            .query_row(
                "SELECT id, run_id, external_change_id, status, start_event_id,
                        end_event_id, started_at, ended_at, created_at, updated_at
                 FROM run_boundaries WHERE id = ?1",
                params![boundary_id],
                row_to_run_boundary,
            )
            .optional()
            .context("failed to query reported change boundary")
    }

    pub fn get_run_boundary_paths(&self, boundary_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM run_boundary_paths
             WHERE boundary_id = ?1 ORDER BY path",
        )?;
        let paths = stmt.query_map(params![boundary_id], |row| row.get::<_, String>(0))?;
        paths
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query reported boundary paths")
    }

    fn ensure_boundary_paths(&self, boundary_id: i64, requested: &BTreeSet<String>) -> Result<()> {
        let stored = self
            .get_run_boundary_paths(boundary_id)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        if &stored != requested {
            anyhow::bail!("idempotent replay for this change used a different exact path set");
        }
        Ok(())
    }

    fn validated_boundary_paths(
        &self,
        run: &Session,
        paths: &[String],
    ) -> Result<BTreeSet<String>> {
        let requested = paths.iter().cloned().collect::<BTreeSet<_>>();
        if requested.is_empty() || requested.len() != paths.len() {
            anyhow::bail!("a reported change requires a non-empty unique path set");
        }
        let project = self
            .get_project_by_id(run.project_id)?
            .ok_or_else(|| anyhow::anyhow!("project {} not found", run.project_id))?;
        let root = Path::new(&project.root_path);
        for path in &requested {
            let path_value = Path::new(path);
            if !path_value.is_absolute()
                || path_value.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
                || !path_value.starts_with(root)
            {
                anyhow::bail!(
                    "reported claim path '{}' must be a normalized absolute path inside {}",
                    path,
                    project.root_path
                );
            }
        }
        Ok(requested)
    }

    pub fn count_run_claimed_events(&self, run_id: i64) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(DISTINCT event_id) FROM session_events WHERE session_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .context("failed to count Run claims")
    }

    pub fn event_claim_count(&self, event_id: i64) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(DISTINCT session_id) FROM session_events WHERE event_id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .context("failed to count event claims")
    }

    pub fn get_event_claim_counts(
        &self,
        project_id: i64,
        first_event_id: i64,
        last_event_id: i64,
    ) -> Result<HashMap<i64, usize>> {
        let mut stmt = self.conn.prepare(
            "SELECT se.event_id, COUNT(DISTINCT se.session_id)
             FROM session_events se
             JOIN file_events e ON e.id = se.event_id
             WHERE e.project_id = ?1
               AND e.id >= ?2
               AND e.id <= ?3
             GROUP BY se.event_id",
        )?;
        let rows = stmt.query_map(params![project_id, first_event_id, last_event_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>()
            .context("failed to query event claim counts")
    }

    /// Classify whether whole-file recovery is safe for one reported Run path.
    /// "Exclusive" means exclusive integration claims, not process provenance.
    pub fn classify_run_path_ownership(&self, run_id: i64, path: &str) -> Result<String> {
        let run = self
            .get_session_by_id(run_id)?
            .ok_or_else(|| anyhow::anyhow!("Run r_{run_id} not found"))?;
        let claimed: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT e.id)
             FROM file_events e
             JOIN session_events mine
               ON mine.event_id = e.id AND mine.session_id = ?1
             WHERE e.project_id = ?2 AND (e.path = ?3 OR e.old_path = ?3)",
            params![run.id, run.project_id, path],
            |row| row.get(0),
        )?;
        if claimed == 0 {
            return Ok("unattributed".to_string());
        }
        let collision: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM file_events e
             JOIN session_events mine
               ON mine.event_id = e.id AND mine.session_id = ?1
             WHERE e.project_id = ?2
               AND (e.path = ?3 OR e.old_path = ?3)
               AND (SELECT COUNT(DISTINCT se.session_id)
                    FROM session_events se WHERE se.event_id = e.id) > 1",
            params![run.id, run.project_id, path],
            |row| row.get(0),
        )?;
        if collision > 0 {
            return Ok("collision".to_string());
        }
        let foreign_or_unattributed: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM file_events e
             WHERE e.project_id = ?1
               AND e.id > ?2
               AND (e.path = ?3 OR e.old_path = ?3)
               AND NOT (
                    (SELECT COUNT(DISTINCT se.session_id)
                     FROM session_events se WHERE se.event_id = e.id) = 1
                    AND EXISTS (
                        SELECT 1 FROM session_events mine
                        WHERE mine.event_id = e.id AND mine.session_id = ?4
                    )
               )",
            params![run.project_id, run.start_event_id, path, run.id],
            |row| row.get(0),
        )?;
        Ok(if foreign_or_unattributed > 0 {
            "interleaved"
        } else {
            "exclusive"
        }
        .to_string())
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

    pub fn max_event_id(&self, project_id: i64) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM file_events WHERE project_id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .context("failed to query max event id")
    }

    /// When the user most recently applied a recovery in this project.
    /// The web UI uses it to retire panic alerts for bursts that were
    /// already addressed.
    pub fn latest_applied_recovery_at(&self, project_id: i64) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT MAX(applied_at) FROM recoveries
                 WHERE project_id = ?1 AND status = 'applied'",
                params![project_id],
                |row| row.get(0),
            )
            .context("failed to query latest applied recovery")
    }

    // ── checkpoint operations ───────────────────────────────────────

    pub fn create_checkpoint(&self, project_id: i64, name: &str, timestamp: i64) -> Result<()> {
        self.immediate_transaction(|db| {
            let run_id = db.get_active_session(project_id)?.map(|run| run.id);
            let event_id = db.max_event_id(project_id)?;
            db.create_checkpoint_at(project_id, run_id, name, timestamp, event_id, None)?;
            Ok(())
        })
    }

    pub fn create_checkpoint_now(
        &self,
        project_id: i64,
        run_id: Option<i64>,
        name: &str,
        intent: Option<&str>,
    ) -> Result<(Checkpoint, bool)> {
        self.immediate_transaction(|db| {
            let timestamp = Utc::now().timestamp();
            let event_id = db.max_event_id(project_id)?;
            db.create_checkpoint_at(project_id, run_id, name, timestamp, event_id, intent)
        })
    }

    fn create_checkpoint_at(
        &self,
        project_id: i64,
        run_id: Option<i64>,
        name: &str,
        timestamp: i64,
        event_id: i64,
        intent: Option<&str>,
    ) -> Result<(Checkpoint, bool)> {
        let existing = match run_id {
            Some(run_id) => self.get_checkpoint_for_run(run_id, name)?,
            None => self.get_legacy_checkpoint(project_id, name)?,
        };
        if let Some(existing) = existing {
            return Ok((existing, false));
        }
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO checkpoints
                (project_id, run_id, name, timestamp, event_id, intent, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![project_id, run_id, name, timestamp, event_id, intent, now],
        )?;
        let checkpoint = self
            .get_checkpoint_by_id(self.conn.last_insert_rowid())?
            .ok_or_else(|| anyhow::anyhow!("failed to read checkpoint after creating it"))?;
        Ok((checkpoint, true))
    }

    pub fn list_checkpoints(&self, project_id: i64) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, run_id, name, timestamp, event_id, intent, created_at
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
                "SELECT id, project_id, run_id, name, timestamp, event_id, intent, created_at
                 FROM checkpoints
                 WHERE project_id = ?1 AND name = ?2
                 ORDER BY timestamp DESC, id DESC
                 LIMIT 1",
                params![project_id, name],
                row_to_checkpoint,
            )
            .optional()
            .context("failed to query checkpoint")
    }

    fn get_checkpoint_for_run(&self, run_id: i64, name: &str) -> Result<Option<Checkpoint>> {
        self.conn
            .query_row(
                "SELECT id, project_id, run_id, name, timestamp, event_id, intent, created_at
                 FROM checkpoints
                 WHERE run_id = ?1 AND name = ?2
                 LIMIT 1",
                params![run_id, name],
                row_to_checkpoint,
            )
            .optional()
            .context("failed to query Run checkpoint")
    }

    fn get_legacy_checkpoint(&self, project_id: i64, name: &str) -> Result<Option<Checkpoint>> {
        self.conn
            .query_row(
                "SELECT id, project_id, run_id, name, timestamp, event_id, intent, created_at
                 FROM checkpoints
                 WHERE project_id = ?1 AND run_id IS NULL AND name = ?2
                 LIMIT 1",
                params![project_id, name],
                row_to_checkpoint,
            )
            .optional()
            .context("failed to query legacy checkpoint")
    }

    pub fn get_checkpoint_by_ref(
        &self,
        project_id: i64,
        reference: &str,
    ) -> Result<Option<Checkpoint>> {
        if let Some(id) = parse_public_id(reference, "cp_") {
            return self
                .get_checkpoint_by_id(id)
                .map(|checkpoint| checkpoint.filter(|cp| cp.project_id == project_id));
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, run_id, name, timestamp, event_id, intent, created_at
             FROM checkpoints
             WHERE project_id = ?1 AND name = ?2
             ORDER BY timestamp DESC, id DESC
             LIMIT 2",
        )?;
        let checkpoints = stmt
            .query_map(params![project_id, reference], row_to_checkpoint)?
            .collect::<Result<Vec<_>, _>>()?;
        match checkpoints.as_slice() {
            [] => Ok(None),
            [checkpoint] => Ok(Some(checkpoint.clone())),
            [first, second] => anyhow::bail!(
                "checkpoint name '{}' is ambiguous ({} and {}). Use a specific checkpoint id, \
                 such as {}.",
                reference,
                first.public_id(),
                second.public_id(),
                first.public_id()
            ),
            _ => unreachable!("query is limited to two checkpoints"),
        }
    }

    fn get_checkpoint_by_id(&self, checkpoint_id: i64) -> Result<Option<Checkpoint>> {
        self.conn
            .query_row(
                "SELECT id, project_id, run_id, name, timestamp, event_id, intent, created_at
                 FROM checkpoints
                 WHERE id = ?1
                 LIMIT 1",
                params![checkpoint_id],
                row_to_checkpoint,
            )
            .optional()
            .context("failed to query checkpoint by id")
    }

    // ── Run intent operations ───────────────────────────────────────

    pub fn start_run_intent(&self, run_id: i64, label: &str) -> Result<RunIntent> {
        self.immediate_transaction(|db| {
            let run = db
                .get_session_by_id(run_id)?
                .ok_or_else(|| anyhow::anyhow!("Run r_{} not found.", run_id))?;
            if run.ended_at.is_some() {
                anyhow::bail!("Run {} is already completed.", run.public_id());
            }
            let event_id = db.max_event_id(run.project_id)?;
            db.start_run_intent_at(run_id, label, event_id, Utc::now().timestamp())
        })
    }

    fn start_run_intent_at(
        &self,
        run_id: i64,
        label: &str,
        event_id: i64,
        timestamp: i64,
    ) -> Result<RunIntent> {
        if let Some(active) = self.get_active_run_intent(run_id)? {
            if active.label == label {
                return Ok(active);
            }
            anyhow::bail!(
                "Run r_{} already has an active intent. Complete it before starting another.",
                run_id
            );
        }
        self.conn.execute(
            "INSERT INTO run_intents
                (run_id, label, status, start_event_id, started_at)
             VALUES (?1, ?2, 'active', ?3, ?4)",
            params![run_id, label, event_id, timestamp],
        )?;
        self.get_run_intent_by_id(self.conn.last_insert_rowid())?
            .ok_or_else(|| anyhow::anyhow!("failed to read intent after creating it"))
    }

    pub fn complete_run_intent(&self, run_id: i64, label: Option<&str>) -> Result<RunIntent> {
        self.immediate_transaction(|db| {
            let run = db
                .get_session_by_id(run_id)?
                .ok_or_else(|| anyhow::anyhow!("Run r_{} not found.", run_id))?;
            if run.ended_at.is_some() {
                anyhow::bail!("Run {} is already completed.", run.public_id());
            }
            let event_id = db.max_event_id(run.project_id)?;
            db.complete_run_intent_at(run_id, label, event_id, Utc::now().timestamp())
        })
    }

    fn complete_run_intent_at(
        &self,
        run_id: i64,
        label: Option<&str>,
        event_id: i64,
        timestamp: i64,
    ) -> Result<RunIntent> {
        let Some(intent) = self.get_active_run_intent(run_id)? else {
            if let Some(label) = label
                && let Some(completed) = self
                    .list_run_intents(run_id)?
                    .into_iter()
                    .rev()
                    .find(|intent| intent.label == label && intent.ended_at.is_some())
            {
                return Ok(completed);
            }
            anyhow::bail!(
                "Run r_{} has no active intent. Send intent_started before intent_completed.",
                run_id
            );
        };
        if let Some(label) = label
            && intent.label != label
        {
            anyhow::bail!(
                "Run r_{} has active intent '{}', not '{}'. Send the active intent name or omit \
                 'intent'.",
                run_id,
                intent.label,
                label
            );
        }
        self.conn.execute(
            "UPDATE run_intents
             SET status = 'completed', end_event_id = ?1, ended_at = ?2
             WHERE id = ?3",
            params![event_id, timestamp, intent.id],
        )?;
        self.get_run_intent_by_id(intent.id)?
            .ok_or_else(|| anyhow::anyhow!("failed to read completed intent"))
    }

    pub fn list_run_intents(&self, run_id: i64) -> Result<Vec<RunIntent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, label, status, start_event_id, end_event_id,
                    started_at, ended_at
             FROM run_intents
             WHERE run_id = ?1
             ORDER BY start_event_id, id",
        )?;
        let intents = stmt.query_map(params![run_id], row_to_run_intent)?;
        intents
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query Run intents")
    }

    pub fn get_active_run_intent(&self, run_id: i64) -> Result<Option<RunIntent>> {
        self.conn
            .query_row(
                "SELECT id, run_id, label, status, start_event_id, end_event_id,
                        started_at, ended_at
                 FROM run_intents
                 WHERE run_id = ?1 AND ended_at IS NULL
                 ORDER BY id DESC
                 LIMIT 1",
                params![run_id],
                row_to_run_intent,
            )
            .optional()
            .context("failed to query active Run intent")
    }

    fn get_run_intent_by_id(&self, intent_id: i64) -> Result<Option<RunIntent>> {
        self.conn
            .query_row(
                "SELECT id, run_id, label, status, start_event_id, end_event_id,
                        started_at, ended_at
                 FROM run_intents
                 WHERE id = ?1",
                params![intent_id],
                row_to_run_intent,
            )
            .optional()
            .context("failed to query Run intent")
    }

    pub fn get_events_between_ids(
        &self,
        project_id: i64,
        start_event_id: i64,
        end_event_id: i64,
    ) -> Result<Vec<FileEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, timestamp, path, event_type,
                    current_hash, previous_hash, snapshot_path, old_path, file_size
             FROM file_events
             WHERE project_id = ?1 AND id > ?2 AND id <= ?3
             ORDER BY id",
        )?;
        let events = stmt.query_map(
            params![project_id, start_event_id, end_event_id],
            row_to_event,
        )?;
        events
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query events between boundaries")
    }

    // ── Recovery operations ─────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn create_recovery(
        &self,
        project_id: i64,
        run_id: Option<i64>,
        request: &str,
        kind: &str,
        confidence: &str,
        ambiguity: Option<&str>,
        entries: &[RecoveryEntry],
    ) -> Result<Recovery> {
        let now = Utc::now().timestamp();
        let expires_at = now.saturating_add(24 * 60 * 60);
        self.transaction(|db| {
            db.conn.execute(
                "INSERT INTO recoveries
                    (project_id, run_id, request, kind, status, confidence,
                     ambiguity, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, 'planned', ?5, ?6, ?7, ?8)",
                params![
                    project_id, run_id, request, kind, confidence, ambiguity, now, expires_at
                ],
            )?;
            let recovery_id = db.conn.last_insert_rowid();
            for entry in entries {
                db.conn.execute(
                    "INSERT INTO recovery_entries
                        (recovery_id, path, action, target_hash, source_timestamp,
                         expected_hash, expected_exists)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        recovery_id,
                        entry.path,
                        entry.action,
                        entry.target_hash,
                        entry.source_timestamp,
                        entry.expected_hash,
                        entry.expected_exists as i32
                    ],
                )?;
            }
            Ok(recovery_id)
        })
        .and_then(|id| {
            self.get_recovery_by_id(id)?
                .ok_or_else(|| anyhow::anyhow!("failed to read Recovery after creating it"))
        })
    }

    pub fn get_recovery_by_ref(
        &self,
        project_id: i64,
        reference: &str,
    ) -> Result<Option<Recovery>> {
        let Some(id) = parse_public_id(reference, "rec_") else {
            return Ok(None);
        };
        self.get_recovery_by_id(id)
            .map(|recovery| recovery.filter(|recovery| recovery.project_id == project_id))
    }

    pub fn get_recovery_entries(&self, recovery_id: i64) -> Result<Vec<RecoveryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT recovery_id, path, action, target_hash, source_timestamp,
                    expected_hash, expected_exists
             FROM recovery_entries
             WHERE recovery_id = ?1
             ORDER BY path",
        )?;
        let entries = stmt.query_map(params![recovery_id], row_to_recovery_entry)?;
        entries
            .collect::<Result<Vec<_>, _>>()
            .context("failed to query Recovery entries")
    }

    pub fn mark_recovery_applied(&self, recovery_id: i64) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "UPDATE recoveries
             SET status = 'applied', applied_at = ?1
             WHERE id = ?2 AND status = 'planned'",
            params![now, recovery_id],
        )?;
        Ok(())
    }

    pub fn mark_recovery_conflicted(&self, recovery_id: i64, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE recoveries
             SET status = 'conflicted', ambiguity = ?1
             WHERE id = ?2 AND status = 'planned'",
            params![reason, recovery_id],
        )?;
        Ok(())
    }

    fn get_recovery_by_id(&self, recovery_id: i64) -> Result<Option<Recovery>> {
        self.conn
            .query_row(
                "SELECT id, project_id, run_id, request, kind, status, confidence,
                        ambiguity, created_at, expires_at, applied_at
                 FROM recoveries
                 WHERE id = ?1",
                params![recovery_id],
                row_to_recovery,
            )
            .optional()
            .context("failed to query Recovery")
    }

    // ── Integration idempotency ─────────────────────────────────────

    pub fn get_integration_response(
        &self,
        key: &str,
        request_hash: &str,
    ) -> Result<Option<String>> {
        let existing = self
            .conn
            .query_row(
                "SELECT request_hash, response_json
                 FROM integration_events WHERE idempotency_key = ?1",
                params![key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .context("failed to query integration event")?;
        let Some((stored_hash, response)) = existing else {
            return Ok(None);
        };
        // Rows written before schema v4 have no recoverable request body, so
        // retain their historical replay behavior. Every v4 write is bound to
        // its canonical request hash and rejects key reuse with another body.
        if !stored_hash.is_empty() && stored_hash != request_hash {
            anyhow::bail!(
                "idempotency key '{}' was already used with a different payload",
                key
            );
        }
        Ok(Some(response))
    }

    pub fn record_integration_response(
        &self,
        key: &str,
        request_hash: &str,
        run_id: Option<i64>,
        event_type: &str,
        response_json: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO integration_events
                (idempotency_key, run_id, event_type, request_hash, response_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                key,
                run_id,
                event_type,
                request_hash,
                response_json,
                Utc::now().timestamp()
            ],
        )?;
        Ok(())
    }
}

fn parse_public_id(value: &str, prefix: &str) -> Option<i64> {
    value.strip_prefix(prefix)?.parse().ok()
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
        run_id: row.get(2)?,
        name: row.get(3)?,
        timestamp: row.get(4)?,
        event_id: row.get(5)?,
        intent: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        kind: row.get(3)?,
        attribution_mode: row.get(4)?,
        actor: row.get(5)?,
        agent: row.get(6)?,
        command: row.get(7)?,
        intent: row.get(8)?,
        external_id: row.get(9)?,
        status: row.get(10)?,
        started_at: row.get(11)?,
        ended_at: row.get(12)?,
        start_event_id: row.get(13)?,
        end_event_id: row.get(14)?,
        created_at: row.get(15)?,
    })
}

fn row_to_run_boundary(row: &rusqlite::Row) -> rusqlite::Result<RunBoundary> {
    Ok(RunBoundary {
        id: row.get(0)?,
        run_id: row.get(1)?,
        external_change_id: row.get(2)?,
        status: row.get(3)?,
        start_event_id: row.get(4)?,
        end_event_id: row.get(5)?,
        started_at: row.get(6)?,
        ended_at: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn row_to_run_intent(row: &rusqlite::Row) -> rusqlite::Result<RunIntent> {
    Ok(RunIntent {
        id: row.get(0)?,
        run_id: row.get(1)?,
        label: row.get(2)?,
        status: row.get(3)?,
        start_event_id: row.get(4)?,
        end_event_id: row.get(5)?,
        started_at: row.get(6)?,
        ended_at: row.get(7)?,
    })
}

fn row_to_recovery(row: &rusqlite::Row) -> rusqlite::Result<Recovery> {
    Ok(Recovery {
        id: row.get(0)?,
        project_id: row.get(1)?,
        run_id: row.get(2)?,
        request: row.get(3)?,
        kind: row.get(4)?,
        status: row.get(5)?,
        confidence: row.get(6)?,
        ambiguity: row.get(7)?,
        created_at: row.get(8)?,
        expires_at: row.get(9)?,
        applied_at: row.get(10)?,
    })
}

fn row_to_recovery_entry(row: &rusqlite::Row) -> rusqlite::Result<RecoveryEntry> {
    Ok(RecoveryEntry {
        recovery_id: row.get(0)?,
        path: row.get(1)?,
        action: row.get(2)?,
        target_hash: row.get(3)?,
        source_timestamp: row.get(4)?,
        expected_hash: row.get(5)?,
        expected_exists: row.get::<_, i32>(6)? != 0,
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

    #[test]
    fn get_events_since_limited_returns_newest_events_within_window() {
        let db = db();
        let p = project(&db);
        for (path, timestamp) in [
            ("/home/user/project/outside-window.rs", 99),
            ("/home/user/project/first.rs", 100),
            ("/home/user/project/second.rs", 101),
            ("/home/user/project/third.rs", 102),
        ] {
            db.insert_event_at(p.id, path, "MODIFIED", timestamp)
                .unwrap();
        }

        let events = db.get_events_since_limited(p.id, 100, 2).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, "/home/user/project/third.rs");
        assert_eq!(events[1].path, "/home/user/project/second.rs");
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

    /// Repeating a checkpoint is idempotent: agent retries must never move a
    /// recovery boundary forward.
    #[test]
    fn checkpoint_create_is_idempotent() {
        let db = db();
        let p = project(&db);
        db.create_checkpoint(p.id, "before refactor", 100).unwrap();
        db.create_checkpoint(p.id, "before refactor", 200).unwrap();

        let checkpoint = db
            .get_checkpoint(p.id, "before refactor")
            .unwrap()
            .expect("checkpoint exists");
        assert_eq!(checkpoint.timestamp, 100);

        let checkpoints = db.list_checkpoints(p.id).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].name, "before refactor");
    }

    #[test]
    fn checkpoint_names_can_repeat_across_runs() {
        let db = db();
        let p = project(&db);
        let first = db
            .start_run(
                p.id,
                "first",
                "run",
                "agent",
                Some("Claude Code"),
                None,
                None,
                None,
            )
            .unwrap();
        let (first_checkpoint, _) = db
            .create_checkpoint_at(p.id, Some(first.id), "validated", 100, 0, None)
            .unwrap();
        db.complete_run(first.id, "completed").unwrap();

        let second = db
            .start_run(
                p.id,
                "second",
                "run",
                "agent",
                Some("Codex"),
                None,
                None,
                None,
            )
            .unwrap();
        let (second_checkpoint, _) = db
            .create_checkpoint_at(p.id, Some(second.id), "validated", 200, 0, None)
            .unwrap();

        assert_ne!(first_checkpoint.id, second_checkpoint.id);
        assert_eq!(first_checkpoint.name, second_checkpoint.name);
        let error = db
            .get_checkpoint_by_ref(p.id, "validated")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains(&first_checkpoint.public_id()), "{error}");
        assert!(error.contains(&second_checkpoint.public_id()), "{error}");
        assert_eq!(
            db.get_checkpoint_by_ref(p.id, &second_checkpoint.public_id())
                .unwrap()
                .unwrap()
                .id,
            second_checkpoint.id
        );
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

    #[test]
    fn run_lifecycle_preserves_agent_identity_and_status() {
        let db = db();
        let p = project(&db);
        let started = db
            .start_run(
                p.id,
                "dashboard",
                "run",
                "agent",
                Some("Claude Code"),
                Some("claude"),
                Some("Redesign dashboard"),
                Some("external-42"),
            )
            .unwrap();
        assert_eq!(started.public_id(), format!("r_{}", started.id));
        assert_eq!(started.actor, "agent");
        assert_eq!(started.agent.as_deref(), Some("Claude Code"));
        assert_eq!(started.status, "active");

        let completed = db.complete_run(started.id, "completed").unwrap();
        assert_eq!(completed.status, "completed");
        assert!(completed.ended_at.is_some());

        let retry = db
            .start_run(
                p.id,
                "ignored-retry-name",
                "run",
                "agent",
                Some("Claude Code"),
                None,
                None,
                Some("external-42"),
            )
            .unwrap();
        assert_eq!(retry.id, completed.id);
    }

    #[test]
    fn missing_run_error_points_to_run_list() {
        let error = db().complete_run(999, "completed").unwrap_err().to_string();

        assert!(error.contains("Run r_999 not found"), "{error}");
        assert!(error.contains("undo run list"), "{error}");
    }

    #[test]
    fn intent_errors_explain_the_next_event() {
        let db = db();
        let p = project(&db);
        let run = db.start_session(p.id, "work", "manual").unwrap();
        db.start_run_intent_at(run.id, "first task", 0, 10).unwrap();

        let already_active = db
            .start_run_intent_at(run.id, "second task", 0, 11)
            .unwrap_err()
            .to_string();
        assert!(already_active.contains("already has an active intent"));
        assert!(already_active.contains("Complete it before starting another"));

        let wrong_intent = db
            .complete_run_intent_at(run.id, Some("second task"), 0, 12)
            .unwrap_err()
            .to_string();
        assert!(wrong_intent.contains("active intent 'first task'"));
        assert!(wrong_intent.contains("omit 'intent'"));

        db.complete_run_intent_at(run.id, None, 0, 13).unwrap();
        let no_active = db
            .complete_run_intent_at(run.id, None, 0, 14)
            .unwrap_err()
            .to_string();
        assert!(no_active.contains("Send intent_started before intent_completed"));
    }

    #[test]
    fn run_completion_closes_active_intent_at_the_same_boundary() {
        let db = db();
        let p = project(&db);
        let run = db.start_session(p.id, "intent-boundary", "manual").unwrap();
        db.start_run_intent(run.id, "edit auth").unwrap();
        db.insert_event(
            p.id,
            "/home/user/project/src/auth.rs",
            "MODIFIED",
            Some("new_hash"),
            Some("old_hash"),
            None,
            None,
            Some(10),
        )
        .unwrap();

        let completed = db.complete_run(run.id, "completed").unwrap();
        let intents = db.list_run_intents(run.id).unwrap();

        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].end_event_id, completed.end_event_id);
        assert_eq!(intents[0].ended_at, completed.ended_at);
    }

    #[test]
    fn checkpoint_now_captures_the_latest_committed_event() {
        let db = db();
        let p = project(&db);
        db.insert_event(
            p.id,
            "/home/user/project/src/checkpoint.rs",
            "MODIFIED",
            Some("new_hash"),
            None,
            None,
            None,
            Some(10),
        )
        .unwrap();

        let (checkpoint, created) = db
            .create_checkpoint_now(p.id, None, "latest", None)
            .unwrap();

        assert!(created);
        assert_eq!(checkpoint.event_id, Some(db.max_event_id(p.id).unwrap()));
    }

    #[test]
    fn recovery_plan_and_entries_round_trip() {
        let db = db();
        let p = project(&db);
        let recovery = db
            .create_recovery(
                p.id,
                None,
                "remove auth",
                "intent",
                "explicit-intent",
                None,
                &[RecoveryEntry {
                    recovery_id: 0,
                    path: "/home/user/project/auth.rs".to_string(),
                    action: "WRITE".to_string(),
                    target_hash: Some("before".to_string()),
                    source_timestamp: Some(10),
                    expected_hash: Some("after".to_string()),
                    expected_exists: true,
                }],
            )
            .unwrap();
        assert_eq!(recovery.status, "planned");
        let entries = db.get_recovery_entries(recovery.id).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].expected_hash.as_deref(), Some("after"));

        db.mark_recovery_applied(recovery.id).unwrap();
        let applied = db
            .get_recovery_by_ref(p.id, &recovery.public_id())
            .unwrap()
            .unwrap();
        assert_eq!(applied.status, "applied");
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

    #[test]
    fn two_reported_runs_can_be_active_and_singleton_lookup_is_ambiguous() {
        let db = db();
        let p = project(&db);
        let first = db
            .start_reported_run(
                p.id,
                "first",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:first",
            )
            .unwrap();
        let second = db
            .start_reported_run(
                p.id,
                "second",
                "hook",
                "agent",
                Some("Claude Code"),
                None,
                None,
                "claude:second",
            )
            .unwrap();

        let active = db.list_active_runs(p.id).unwrap();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(Session::is_reported));
        let error = db.get_active_session(p.id).unwrap_err().to_string();
        assert!(error.contains("multiple Runs are active"), "{error}");
        assert!(error.contains(&first.public_id()), "{error}");
        assert!(error.contains(&second.public_id()), "{error}");
    }

    #[test]
    fn window_and_reported_active_runs_cannot_mix() {
        let db = db();
        let p = project(&db);
        let reported = db
            .start_reported_run(
                p.id,
                "reported",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:reported",
            )
            .unwrap();
        assert!(db.start_session(p.id, "window", "manual").is_err());
        db.complete_run(reported.id, "completed").unwrap();

        db.start_session(p.id, "window", "manual").unwrap();
        let error = db
            .start_reported_run(
                p.id,
                "reported-two",
                "hook",
                "agent",
                Some("Codex"),
                None,
                None,
                "codex:reported",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("window attribution"), "{error}");
    }

    #[test]
    fn reported_run_without_claims_has_no_interval_fallback() {
        let db = db();
        let p = project(&db);
        let run = db
            .start_reported_run(
                p.id,
                "reported",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:zero",
            )
            .unwrap();
        let unclaimed = "/home/user/project/unclaimed.rs".to_string();
        db.open_run_boundary(run.id, "left-open", std::slice::from_ref(&unclaimed))
            .unwrap();
        db.insert_event(
            p.id,
            &unclaimed,
            "MODIFIED",
            Some("new"),
            Some("old"),
            None,
            None,
            Some(1),
        )
        .unwrap();
        let completed = db.complete_run(run.id, "completed").unwrap();

        assert!(db.get_session_events(&completed).unwrap().is_empty());
        assert_eq!(db.count_run_claimed_events(run.id).unwrap(), 0);
        assert_eq!(
            db.get_run_boundary_by_external_id(run.id, "left-open")
                .unwrap()
                .unwrap()
                .status,
            "aborted"
        );
    }

    #[test]
    fn overlapping_reported_claims_are_collision_and_disjoint_claims_are_exclusive() {
        let db = db();
        let p = project(&db);
        let first = db
            .start_reported_run(
                p.id,
                "first",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:collision",
            )
            .unwrap();
        let second = db
            .start_reported_run(
                p.id,
                "second",
                "hook",
                "agent",
                Some("Codex"),
                None,
                None,
                "codex:collision",
            )
            .unwrap();
        let shared = "/home/user/project/shared.rs".to_string();
        db.open_run_boundary(first.id, "first-shared", std::slice::from_ref(&shared))
            .unwrap();
        db.open_run_boundary(second.id, "second-shared", std::slice::from_ref(&shared))
            .unwrap();
        db.insert_event(
            p.id,
            &shared,
            "MODIFIED",
            Some("new"),
            Some("old"),
            None,
            None,
            Some(1),
        )
        .unwrap();
        db.close_run_boundary(first.id, "first-shared", std::slice::from_ref(&shared))
            .unwrap();
        db.close_run_boundary(second.id, "second-shared", std::slice::from_ref(&shared))
            .unwrap();
        let event_id = db.max_event_id(p.id).unwrap();
        assert_eq!(db.event_claim_count(event_id).unwrap(), 2);
        assert_eq!(
            db.classify_run_path_ownership(first.id, &shared).unwrap(),
            "collision"
        );

        let first_only = "/home/user/project/first.rs".to_string();
        let second_only = "/home/user/project/second.rs".to_string();
        db.open_run_boundary(first.id, "first-only", std::slice::from_ref(&first_only))
            .unwrap();
        db.insert_event(
            p.id,
            &first_only,
            "MODIFIED",
            Some("new"),
            Some("old"),
            None,
            None,
            Some(1),
        )
        .unwrap();
        db.close_run_boundary(first.id, "first-only", std::slice::from_ref(&first_only))
            .unwrap();
        db.open_run_boundary(second.id, "second-only", std::slice::from_ref(&second_only))
            .unwrap();
        db.insert_event(
            p.id,
            &second_only,
            "MODIFIED",
            Some("new"),
            Some("old"),
            None,
            None,
            Some(1),
        )
        .unwrap();
        db.close_run_boundary(second.id, "second-only", std::slice::from_ref(&second_only))
            .unwrap();

        assert_eq!(
            db.classify_run_path_ownership(first.id, &first_only)
                .unwrap(),
            "exclusive"
        );
        assert_eq!(
            db.classify_run_path_ownership(second.id, &second_only)
                .unwrap(),
            "exclusive"
        );
    }

    #[test]
    fn reported_claim_matches_rename_old_path() {
        let db = db();
        let p = project(&db);
        let run = db
            .start_reported_run(
                p.id,
                "rename",
                "hook",
                "agent",
                Some("Cursor"),
                None,
                None,
                "cursor:rename",
            )
            .unwrap();
        let old = "/home/user/project/old.rs".to_string();
        let new = "/home/user/project/new.rs".to_string();
        db.open_run_boundary(run.id, "rename-tool", std::slice::from_ref(&old))
            .unwrap();
        db.insert_event(
            p.id,
            &new,
            "RENAMED",
            Some("hash"),
            Some("hash"),
            None,
            Some(&old),
            Some(1),
        )
        .unwrap();
        db.close_run_boundary(run.id, "rename-tool", std::slice::from_ref(&old))
            .unwrap();

        let events = db.get_session_events(&run).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].old_path.as_deref(), Some(old.as_str()));
    }

    #[test]
    fn integration_idempotency_rejects_hash_mismatch_and_replays_match() {
        let db = db();
        db.record_integration_response("key-1", "hash-a", None, "run_started", r#"{"ok":true}"#)
            .unwrap();
        assert_eq!(
            db.get_integration_response("key-1", "hash-a")
                .unwrap()
                .as_deref(),
            Some(r#"{"ok":true}"#)
        );
        let error = db
            .get_integration_response("key-1", "hash-b")
            .unwrap_err()
            .to_string();
        assert!(error.contains("different payload"), "{error}");
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

    #[test]
    fn run_completion_waits_for_an_inflight_event_commit() {
        use std::sync::mpsc;
        use std::time::Duration;

        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let db = Database::open().unwrap();
        let p = project(&db);
        let run = db.start_session(p.id, "concurrent-stop", "manual").unwrap();
        let writer_db = Database::open().unwrap();
        let stopper_db = Database::open().unwrap();
        let project_id = p.id;

        let (inserted_tx, inserted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_db
                .immediate_transaction(|db| {
                    db.insert_event(
                        project_id,
                        "/home/user/project/src/concurrent.rs",
                        "MODIFIED",
                        Some("new_hash"),
                        Some("old_hash"),
                        None,
                        None,
                        Some(10),
                    )?;
                    inserted_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });

        inserted_rx.recv().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let run_id = run.id;
        let stopper = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            finished_tx
                .send(stopper_db.complete_run(run_id, "completed"))
                .unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            finished_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "Run completion must wait for the active event writer"
        );
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        let completed = finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Run completion did not resume after the event committed")
            .unwrap();
        stopper.join().unwrap();

        let committed_event_id = db.max_event_id(project_id).unwrap();
        assert_eq!(completed.end_event_id, Some(committed_event_id));
        let events = db.get_session_events(&completed).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "/home/user/project/src/concurrent.rs");
    }

    #[test]
    fn run_start_waits_for_an_inflight_baseline_event() {
        use std::sync::mpsc;
        use std::time::Duration;

        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let db = Database::open().unwrap();
        let p = project(&db);
        let writer_db = Database::open().unwrap();
        let starter_db = Database::open().unwrap();
        let project_id = p.id;

        let (inserted_tx, inserted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_db
                .immediate_transaction(|db| {
                    db.insert_event(
                        project_id,
                        "/home/user/project/src/baseline.rs",
                        "MODIFIED",
                        Some("baseline_hash"),
                        None,
                        None,
                        None,
                        Some(10),
                    )?;
                    inserted_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });

        inserted_rx.recv().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let starter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            finished_tx
                .send(starter_db.start_session(project_id, "concurrent-start", "manual"))
                .unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            finished_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "Run start must wait for the active event writer"
        );
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        let started = finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Run start did not resume after the event committed")
            .unwrap();
        starter.join().unwrap();

        assert_eq!(started.start_event_id, db.max_event_id(project_id).unwrap());
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

    #[test]
    fn migration_preserves_legacy_sessions_and_rebuilds_checkpoint_uniqueness() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE watched_projects (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                root_path TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL
             );
             INSERT INTO watched_projects (id, root_path, created_at)
             VALUES (1, '/legacy', 1);
             CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'manual',
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                start_event_id INTEGER NOT NULL DEFAULT 0,
                end_event_id INTEGER,
                created_at INTEGER NOT NULL,
                UNIQUE(project_id, name)
             );
             INSERT INTO sessions
                (id, project_id, name, kind, started_at, ended_at,
                 start_event_id, end_event_id, created_at)
             VALUES (7, 1, 'legacy-agent-work', 'manual', 10, 20, 0, 0, 10);
             CREATE TABLE checkpoints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE(project_id, name)
             );
             INSERT INTO checkpoints
                (id, project_id, name, timestamp, created_at)
             VALUES (9, 1, 'before', 11, 11);",
        )
        .unwrap();

        apply_schema(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);

        let db = Database { conn };
        let legacy = db.get_session_by_id(7).unwrap().unwrap();
        assert_eq!(legacy.name, "legacy-agent-work");
        assert_eq!(legacy.attribution_mode, "window");
        assert_eq!(legacy.actor, "human");
        assert_eq!(legacy.status, "completed");
        let checkpoint = db.get_checkpoint_by_ref(1, "cp_9").unwrap().unwrap();
        assert_eq!(checkpoint.name, "before");
        assert_eq!(checkpoint.event_id, None);

        let new_run = db
            .start_run(1, "new", "run", "agent", Some("Codex"), None, None, None)
            .unwrap();
        let (new_checkpoint, created) = db
            .create_checkpoint_at(1, Some(new_run.id), "before", 30, 0, None)
            .unwrap();
        assert!(created);
        assert_ne!(new_checkpoint.id, checkpoint.id);
        let active_index: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_sessions_one_active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_index, 0);
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
