import { computed, reactive, readonly } from 'vue'
import type {
  ApplyResult,
  Bootstrap,
  DiffPayload,
  DiffTarget,
  FileChange,
  PanicAlert,
  PollPayload,
  ProjectSummary,
  RecoveryView,
  TimelineItem,
  TimelinePayload,
} from '~/types'
import { fmtClock } from '~/utils/format'

interface Toast {
  id: number
  kind: 'ok' | 'error'
  message: string
}

interface UndoState {
  ready: boolean
  fatal: string | null
  version: string
  projects: ProjectSummary[]
  projectId: number | null
  timeline: TimelinePayload | null
  timelineLoading: boolean
  expanded: Set<string>
  // Per timeline item: project-relative paths marked for undo.
  selections: Map<string, Set<string>>
  diffTarget: DiffTarget | null
  diff: DiffPayload | null
  diffLoading: boolean
  recovery: RecoveryView | null
  recoveryBusy: boolean
  applyResult: ApplyResult | null
  recording: boolean
  activeRunId: string | null
  // Timeline item id to review in isolation (`undo ui r_421` deep link).
  focusId: string | null
  // Panic alert item ids the user dismissed this browser session.
  dismissedAlerts: Set<string>
  toasts: Toast[]
}

const state = reactive<UndoState>({
  ready: false,
  fatal: null,
  version: '',
  projects: [],
  projectId: null,
  timeline: null,
  timelineLoading: false,
  expanded: new Set(),
  selections: new Map(),
  diffTarget: null,
  diff: null,
  diffLoading: false,
  recovery: null,
  recoveryBusy: false,
  applyResult: null,
  recording: false,
  activeRunId: null,
  focusId: null,
  dismissedAlerts: new Set(),
  toasts: [],
})

let token = ''
let pollTimer: ReturnType<typeof setInterval> | null = null
let lastMaxEventId = -1
let toastSeq = 0

function resolveToken(): string {
  const params = new URLSearchParams(window.location.search)
  const fromUrl = params.get('token')
  if (fromUrl) {
    sessionStorage.setItem('undo-token', fromUrl)
    // Keep the token out of the visible URL and browser history. The hash
    // survives: it carries the `#run=` deep-link focus, never the token.
    params.delete('token')
    const query = params.toString()
    const clean =
      window.location.pathname +
      (query ? `?${query}` : '') +
      window.location.hash
    window.history.replaceState(null, '', clean)
    return fromUrl
  }
  return sessionStorage.getItem('undo-token') ?? ''
}

/// `undo ui r_421` opens the app with `#run=r_421`: review that Run in
/// isolation, with the full timeline one click away.
function resolveFocus(): string | null {
  const match = window.location.hash.match(/^#run=([\w-]+)$/)
  return match?.[1] ?? null
}

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`/api${path}`, {
    ...init,
    headers: {
      'X-Undo-Token': token,
      ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
      ...init?.headers,
    },
  })
  const payload = await response.json().catch(() => null)
  if (!response.ok) {
    const message =
      payload && typeof payload.error === 'string'
        ? payload.error
        : `request failed (${response.status})`
    throw new Error(message)
  }
  return payload as T
}

function toast(kind: Toast['kind'], message: string) {
  const id = ++toastSeq
  state.toasts.push({ id, kind, message })
  setTimeout(() => {
    state.toasts = state.toasts.filter((entry) => entry.id !== id)
  }, 5200)
}

async function boot() {
  token = resolveToken()
  state.focusId = resolveFocus()
  if (!token) {
    state.fatal =
      'Missing access token. Open the exact link that `undo ui` printed in your terminal.'
    return
  }
  try {
    const bootstrap = await api<Bootstrap>('/bootstrap')
    state.version = bootstrap.version
    state.projects = bootstrap.projects
    state.projectId =
      bootstrap.current_project_id ?? bootstrap.projects[0]?.id ?? null
    state.ready = true
    if (state.projectId !== null) {
      await refreshTimeline()
      startPolling()
    }
  } catch (error) {
    state.fatal = error instanceof Error ? error.message : String(error)
  }
}

async function refreshTimeline() {
  if (state.projectId === null) return
  state.timelineLoading = state.timeline === null
  try {
    const payload = await api<TimelinePayload>(
      `/projects/${state.projectId}/timeline?limit=600`,
    )
    state.timeline = payload
    state.recording = payload.project.recording
    lastMaxEventId = payload.max_event_id
    // A deep-linked Run opens expanded; drop the focus quietly when the
    // item is not in this project's timeline.
    if (state.focusId !== null) {
      if (payload.items.some((item) => item.id === state.focusId)) {
        state.expanded.add(state.focusId)
      } else {
        clearFocus()
      }
    }
    // Auto-expand the newest item on first load.
    if (state.expanded.size === 0 && payload.items.length > 0) {
      state.expanded.add(payload.items[0]!.id)
    }
  } catch (error) {
    toast('error', error instanceof Error ? error.message : String(error))
  } finally {
    state.timelineLoading = false
  }
}

async function selectProject(id: number) {
  if (state.projectId === id) return
  state.projectId = id
  state.timeline = null
  state.expanded = new Set()
  state.selections = new Map()
  state.diffTarget = null
  state.diff = null
  lastMaxEventId = -1
  await refreshTimeline()
  startPolling()
}

function startPolling() {
  if (pollTimer) clearInterval(pollTimer)
  pollTimer = setInterval(async () => {
    if (state.projectId === null || document.hidden) return
    try {
      const poll = await api<PollPayload>(`/projects/${state.projectId}/poll`)
      state.recording = poll.recording
      state.activeRunId = poll.active_run_id
      if (poll.max_event_id !== lastMaxEventId) {
        lastMaxEventId = poll.max_event_id
        await refreshTimeline()
      }
    } catch {
      // Transient poll failures (server restarting) are not worth a toast.
    }
  }, 2500)
}

function toggleExpanded(itemId: string) {
  if (state.expanded.has(itemId)) state.expanded.delete(itemId)
  else state.expanded.add(itemId)
}

function selectionFor(itemId: string): Set<string> {
  let selection = state.selections.get(itemId)
  if (!selection) {
    selection = new Set()
    state.selections.set(itemId, selection)
  }
  return selection
}

function toggleFile(itemId: string, path: string) {
  const selection = selectionFor(itemId)
  if (selection.has(path)) selection.delete(path)
  else selection.add(path)
  // Reassign to trigger reactivity on Map contents.
  state.selections = new Map(state.selections)
}

function setSelection(itemId: string, paths: string[]) {
  state.selections.set(itemId, new Set(paths))
  state.selections = new Map(state.selections)
}

async function openDiff(itemId: string, file: FileChange) {
  state.diffTarget = { itemId, file }
  state.diff = null
  if (file.binary) return
  state.diffLoading = true
  try {
    const query = new URLSearchParams({
      path: file.path,
      first: String(file.first_event_id),
      last: String(file.last_event_id),
    })
    state.diff = await api<DiffPayload>(
      `/projects/${state.projectId}/diff?${query.toString()}`,
    )
  } catch (error) {
    toast('error', error instanceof Error ? error.message : String(error))
    state.diffTarget = null
  } finally {
    state.diffLoading = false
  }
}

interface UndoRequest {
  item: TimelineItem
  paths: string[]
  description: string
}

async function previewUndo({ item, paths, description }: UndoRequest) {
  if (paths.length === 0) return
  state.recoveryBusy = true
  state.applyResult = null
  try {
    const body =
      item.kind === 'run'
        ? { run_id: item.run_id, paths, request: description }
        : {
            boundary_event_id: item.boundary_event_id,
            paths,
            request: description,
          }
    state.recovery = await api<RecoveryView>(
      `/projects/${state.projectId}/recoveries`,
      { method: 'POST', body: JSON.stringify(body) },
    )
  } catch (error) {
    toast('error', error instanceof Error ? error.message : String(error))
  } finally {
    state.recoveryBusy = false
  }
}

function clearFocus() {
  state.focusId = null
  if (window.location.hash) {
    window.history.replaceState(
      null,
      '',
      window.location.pathname + window.location.search,
    )
  }
}

/// Panic-banner action: restore the whole project to the moment just before
/// the destructive group began. Preview-then-apply like everything else.
async function previewRestoreBefore(alert: PanicAlert) {
  state.recoveryBusy = true
  state.applyResult = null
  try {
    state.recovery = await api<RecoveryView>(
      `/projects/${state.projectId}/recoveries`,
      {
        method: 'POST',
        body: JSON.stringify({
          timestamp: alert.target_timestamp,
          path: '.',
          request: `Restore the project to just before the ${fmtClock(alert.started_at)} changes`,
        }),
      },
    )
  } catch (error) {
    toast('error', error instanceof Error ? error.message : String(error))
  } finally {
    state.recoveryBusy = false
  }
}

function dismissAlert(itemId: string) {
  state.dismissedAlerts.add(itemId)
  state.dismissedAlerts = new Set(state.dismissedAlerts)
}

async function applyRecovery() {
  if (!state.recovery) return
  state.recoveryBusy = true
  try {
    const result = await api<ApplyResult>(
      `/projects/${state.projectId}/recoveries/${state.recovery.id}/apply`,
      { method: 'POST', body: JSON.stringify({}) },
    )
    state.applyResult = result
    toast(
      'ok',
      result.already_applied
        ? `${result.recovery.id} was already applied`
        : `Restored ${result.files_changed} file${result.files_changed === 1 ? '' : 's'}`,
    )
    state.recovery = null
    state.selections = new Map()
    state.diffTarget = null
    state.diff = null
    await refreshTimeline()
  } catch (error) {
    toast('error', error instanceof Error ? error.message : String(error))
  } finally {
    state.recoveryBusy = false
  }
}

function dismissRecovery() {
  state.recovery = null
}

const currentProject = computed(
  () => state.projects.find((project) => project.id === state.projectId) ?? null,
)

/// The deep-linked item, when it exists in the loaded timeline.
const focusedItem = computed(() => {
  if (state.focusId === null) return null
  return (
    state.timeline?.items.find((item) => item.id === state.focusId) ?? null
  )
})

/// The active panic alert, unless the user dismissed it this session.
const panicAlert = computed(() => {
  const alert = state.timeline?.alert ?? null
  if (!alert || state.dismissedAlerts.has(alert.item_id)) return null
  return alert
})

export function useUndo() {
  return {
    state: readonly(state) as Readonly<UndoState>,
    currentProject,
    focusedItem,
    panicAlert,
    boot,
    selectProject,
    refreshTimeline,
    toggleExpanded,
    toggleFile,
    setSelection,
    selectionFor: (itemId: string) => state.selections.get(itemId) ?? new Set<string>(),
    openDiff,
    previewUndo,
    previewRestoreBefore,
    clearFocus,
    dismissAlert,
    applyRecovery,
    dismissRecovery,
    toast,
  }
}
