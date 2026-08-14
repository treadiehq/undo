use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct WatchedProject {
    pub id: i64,
    pub root_path: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
pub struct Checkpoint {
    pub id: i64,
    pub project_id: i64,
    pub run_id: Option<i64>,
    pub name: String,
    pub timestamp: i64,
    pub event_id: Option<i64>,
    pub intent: Option<String>,
    pub created_at: i64,
}

impl Checkpoint {
    pub fn public_id(&self) -> String {
        format!("cp_{}", self.id)
    }
}

/// A bounded period of work. The table is still named `sessions` on disk so
/// existing installations migrate additively without rewriting their history.
#[derive(Clone, Debug, Serialize)]
pub struct Run {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub kind: String,
    /// `window` for legacy/manual/wrapper Runs; `reported` when an integration
    /// explicitly claims exact file-change boundaries. Reported attribution is
    /// an integration claim, not forensic process provenance.
    pub attribution_mode: String,
    pub actor: String,
    pub agent: Option<String>,
    pub command: Option<String>,
    pub intent: Option<String>,
    pub external_id: Option<String>,
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub start_event_id: i64,
    pub end_event_id: Option<i64>,
    pub created_at: i64,
}

impl Run {
    pub fn public_id(&self) -> String {
        format!("r_{}", self.id)
    }

    pub fn is_active(&self) -> bool {
        self.ended_at.is_none()
    }

    pub fn is_reported(&self) -> bool {
        self.attribution_mode == "reported"
    }
}

/// Compatibility name for the pre-Run public API.
pub type Session = Run;

#[derive(Clone, Debug, Serialize)]
pub struct RunBoundary {
    pub id: i64,
    pub run_id: i64,
    pub external_change_id: String,
    pub status: String,
    pub start_event_id: i64,
    pub end_event_id: Option<i64>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunIntent {
    pub id: i64,
    pub run_id: i64,
    pub label: String,
    pub status: String,
    pub start_event_id: i64,
    pub end_event_id: Option<i64>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

impl RunIntent {
    pub fn public_id(&self) -> String {
        format!("i_{}", self.id)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Recovery {
    pub id: i64,
    pub project_id: i64,
    pub run_id: Option<i64>,
    pub request: String,
    pub kind: String,
    pub status: String,
    pub confidence: String,
    pub ambiguity: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub applied_at: Option<i64>,
}

impl Recovery {
    pub fn public_id(&self) -> String {
        format!("rec_{}", self.id)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RecoveryEntry {
    pub recovery_id: i64,
    pub path: String,
    pub action: String,
    pub target_hash: Option<String>,
    pub source_timestamp: Option<i64>,
    pub expected_hash: Option<String>,
    pub expected_exists: bool,
}
