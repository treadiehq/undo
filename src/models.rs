#[derive(Clone, Debug)]
pub struct WatchedProject {
    pub id: i64,
    pub root_path: String,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct FileEvent {
    pub id: i64,
    pub project_id: i64,
    pub timestamp: i64,
    pub path: String,
    pub event_type: String,
    pub current_hash: Option<String>,
    pub previous_hash: Option<String>,
    pub snapshot_path: Option<String>,
    pub old_path: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct FileState {
    pub id: i64,
    pub project_id: i64,
    pub path: String,
    pub latest_hash: Option<String>,
    pub last_seen_at: i64,
    pub exists_now: bool,
    /// On-disk size in bytes at the time the hash was recorded. `None` for rows
    /// written before size/mtime tracking (#26); such rows skip the fast path.
    pub size: Option<i64>,
    /// Modification time in nanoseconds since the Unix epoch when the hash was
    /// recorded. Paired with `size` to short-circuit no-op modify events.
    pub mtime_nanos: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub timestamp: i64,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub kind: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub start_event_id: i64,
    pub end_event_id: Option<i64>,
    pub created_at: i64,
}
