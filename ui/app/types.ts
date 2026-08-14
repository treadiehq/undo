export interface ProjectSummary {
  id: number
  root_path: string
  name: string
  recording: boolean
  event_count: number
  first_event_at: number | null
  last_event_at: number | null
}

export interface Bootstrap {
  version: string
  current_project_id: number | null
  projects: ProjectSummary[]
}

export interface FileChange {
  path: string
  change: 'created' | 'modified' | 'deleted' | 'renamed'
  event_count: number
  first_event_id: number
  last_event_id: number
  last_timestamp: number
  inserted: number
  deleted: number
  binary: boolean
  old_path: string | null
  ownership_status: 'exclusive' | 'interleaved' | 'collision' | 'unattributed'
  recoverable: boolean
  warning: string | null
}

export interface Checkpoint {
  id: number
  project_id: number
  run_id: number | null
  name: string
  timestamp: number
  event_id: number | null
  intent: string | null
  created_at: number
}

export interface TimelineItem {
  id: string
  kind: 'run' | 'collision' | 'edits'
  label: string
  actor: string
  agent: string | null
  command: string | null
  intent: string | null
  status: string
  started_at: number
  ended_at: number | null
  run_id: string | null
  /** "machine" for tool-speed bursts, "human" for hand-paced edits, "run" for Runs. */
  pace: 'machine' | 'human' | 'run'
  /** Dominant directory when most files share one, e.g. "src/auth". */
  scope_hint: string | null
  boundary_event_id: number
  last_event_id: number
  event_count: number
  file_count: number
  inserted: number
  deleted: number
  deleted_files: number
  stats_truncated: boolean
  files: FileChange[]
  checkpoints: Checkpoint[]
}

/** Newest un-attributed group that deleted multiple files recently. */
export interface PanicAlert {
  item_id: string
  started_at: number
  ended_at: number
  file_count: number
  deleted_files: number
  target_timestamp: number
}

export interface TimelinePayload {
  project: ProjectSummary
  items: TimelineItem[]
  checkpoints: Checkpoint[]
  alert: PanicAlert | null
  max_event_id: number
  now: number
}

export interface DiffLine {
  kind: 'ctx' | 'add' | 'del'
  old_line: number | null
  new_line: number | null
  text: string
}

export interface DiffHunk {
  header: string
  lines: DiffLine[]
}

export interface DiffPayload {
  path: string
  change: string
  binary: boolean
  inserted: number
  deleted: number
  old_timestamp: number | null
  new_timestamp: number | null
  hunks: DiffHunk[]
}

export interface RecoveryEntryView {
  path: string
  action: 'WRITE' | 'DELETE'
  source_timestamp: number | null
}

export interface RecoveryView {
  id: string
  status: string
  kind: string
  confidence: string
  request: string
  created_at: number
  expires_at: number
  run_id: string | null
  ambiguity: string | null
  writes: number
  deletes: number
  entries: RecoveryEntryView[]
}

export interface ApplyResult {
  applied: boolean
  already_applied: boolean
  files_changed: number
  recovery: RecoveryView
}

export interface ActiveRunSummary {
  /** Kept for stable identity; never shown as user-facing copy. */
  id: string
  label: string
  started_at: number
}

export interface PollPayload {
  max_event_id: number
  recording: boolean
  active_run_id: string | null
  active_runs: ActiveRunSummary[]
  now: number
}

export interface DiffTarget {
  itemId: string
  file: FileChange
}
