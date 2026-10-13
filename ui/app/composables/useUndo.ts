import { computed, reactive, readonly } from 'vue'
import type {
  ActiveRunSummary,
  ApplyResult,
  Bootstrap,
  DiffPayload,
  DiffTarget,
  FileChange,
  PanicAlert,
  PollPayload,
  ProjectSummary,
  RecoveryView,
  RestorePreviewPayload,
  TimelineItem,
  TimelinePayload,
} from '~/types'
import { fmtClock } from '~/utils/format'
import { createPreviewQueue } from '~/utils/previewQueue'
import { affectedItems, nextRestoreTarget, sameDiffContent } from '~/utils/restoreView'

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
  diffRevision: number
  recovery: RecoveryView | null
  recoveryBusy: boolean
  applyResult: ApplyResult | null
  recording: boolean
  activeRuns: ActiveRunSummary[]
  // Timestamp selected in the restore timeline. Shared so the feed can preview
  // which recorded activity falls after the restore point.
  restoreTimestamp: number | null
  restorePreview: RestorePreviewPayload | null
  restorePreviewLoading: boolean
  restorePreviewError: string | null
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
  diffRevision: 0,
  recovery: null,
  recoveryBusy: false,
  applyResult: null,
  recording: false,
  activeRuns: [],
  restoreTimestamp: null,
  restorePreview: null,
  restorePreviewLoading: false,
  restorePreviewError: null,
  focusId: null,
  dismissedAlerts: new Set(),
  toasts: [],
})

let token = ''
let pollTimer: ReturnType<typeof setInterval> | null = null
let lastMaxEventId = -1
let toastSeq = 0
let diffGeneration = 0

interface PreviewRequest {
  projectId: number
  timestamp: number
  generation: number
}

function previewSourceItems() {
  const items = state.timeline?.items ?? []
  return state.focusId ? items.filter((item) => item.id === state.focusId) : items
}

const visibleTimelineItems = computed(() => {
  const items = previewSourceItems()
  if (state.restoreTimestamp === null) return items
  return state.restorePreview ? affectedItems(items, state.restorePreview.entries) : []
})

const livePreviewQueue = createPreviewQueue<PreviewRequest>(async (request, signal) => {
  const valid = () => !signal.aborted && request.generation === diffGeneration
  async function fetchPreview(path?: string) {
    const query = new URLSearchParams({ timestamp: String(request.timestamp) })
    if (path) query.set('path', path)
    return api<RestorePreviewPayload>(
      `/projects/${request.projectId}/restore-preview?${query.toString()}`, { signal },
    )
  }
  let preview = await fetchPreview(state.diffTarget?.file.path)
  if (!valid()) return
  let items = affectedItems(previewSourceItems(), preview.entries)
  const target = nextRestoreTarget(items, state.diffTarget)
  if (target && preview.diff?.path !== target.file.path) {
    // Resolve the next affected file before committing, so a new filename
    // never appears above the previous file's code.
    preview = await fetchPreview(target.file.path)
    if (!valid()) return
    items = affectedItems(previewSourceItems(), preview.entries)
  }
  const resolved = nextRestoreTarget(items, target)
  const diff = resolved && preview.diff?.path === resolved.file.path ? preview.diff : null
  if (!sameDiffContent(state.diff, diff)) state.diffRevision += 1
  state.restorePreview = preview
  state.diffTarget = resolved
  state.diff = diff
  if (resolved) state.expanded.add(resolved.itemId)
  // A hidden checkbox must never silently remain in a later restore.
  state.selections = new Map(items.map((item) => [
    item.id,
    new Set(item.files.filter((file) => state.selections.get(item.id)?.has(file.path)).map((file) => file.path)),
  ]))
  state.restorePreviewError = null
  state.restorePreviewLoading = request.timestamp !== state.restoreTimestamp
  state.diffLoading = false
}, (error, request) => {
  if (request.generation !== diffGeneration || request.timestamp !== state.restoreTimestamp) return
  state.restorePreviewError = error instanceof Error ? error.message : String(error)
  state.restorePreviewLoading = false
  state.diffLoading = false
})

function cancelLivePreview() {
  diffGeneration += 1
  livePreviewQueue.cancel()
}

function scheduleRestorePreview() {
  const projectId = state.projectId
  const timestamp = state.restoreTimestamp
  state.restorePreviewError = null
  if (projectId === null || timestamp === null) {
    cancelLivePreview()
    state.restorePreview = null
    state.restorePreviewLoading = false
    state.diffLoading = false
    return
  }
  // Keep the last completed view on screen until its replacement is ready.
  // The displayed diff retains its own time label while the thumb moves.
  state.restorePreviewLoading = true
  state.diffLoading = state.diff === null && state.diffTarget !== null
  livePreviewQueue.schedule({ projectId, timestamp, generation: diffGeneration })
}

const restoreEntries = computed(() => state.restorePreview === null ? null : new Map(
  state.restorePreview.entries.map((entry) => [entry.path, entry.action]),
))

function fileRestoreAction(file: FileChange): 'WRITE' | 'DELETE' | null {
  const entries = restoreEntries.value
  if (!entries) return null
  return entries.get(file.path) ?? null
}

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
  const projectId = state.projectId
  if (projectId === null) return
  state.timelineLoading = state.timeline === null
  try {
    const payload = await api<TimelinePayload>(
      `/projects/${projectId}/timeline?limit=600`,
    )
    if (state.projectId !== projectId) return
    state.timeline = payload
    state.recording = payload.project.recording
    state.activeRuns = payload.items
      .filter((item) => item.kind === 'run' && item.status === 'active')
      .map((item) => ({
        id: item.run_id ?? item.id,
        label: item.label,
        started_at: item.started_at,
      }))
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
    cancelLivePreview()
    scheduleRestorePreview()
  } catch (error) {
    if (state.projectId !== projectId) return
    toast('error', error instanceof Error ? error.message : String(error))
  } finally {
    if (state.projectId === projectId) state.timelineLoading = false
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
  state.activeRuns = []
  state.restoreTimestamp = null
  scheduleRestorePreview()
  lastMaxEventId = -1
  await refreshTimeline()
  startPolling()
}

function startPolling() {
  if (pollTimer) clearInterval(pollTimer)
  pollTimer = setInterval(async () => {
    const projectId = state.projectId
    if (projectId === null || document.hidden) return
    try {
      const poll = await api<PollPayload>(`/projects/${projectId}/poll`)
      if (state.projectId !== projectId) return
      state.recording = poll.recording
      state.activeRuns = poll.active_runs
      if (state.timeline) {
        state.timeline = { ...state.timeline, now: poll.now }
      }
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
  cancelLivePreview()
  state.diffTarget = { itemId, file }
  state.diff = null
  if (state.restoreTimestamp !== null) {
    scheduleRestorePreview()
    return
  }
  cancelLivePreview()
  const generation = diffGeneration
  state.diffLoading = false
  if (file.binary) return
  state.diffLoading = true
  try {
    const query = new URLSearchParams({
      path: file.path,
      first: String(file.first_event_id),
      last: String(file.last_event_id),
    })
    const diff = await api<DiffPayload>(
      `/projects/${state.projectId}/diff?${query.toString()}`,
    )
    if (generation === diffGeneration) state.diff = diff
  } catch (error) {
    if (generation !== diffGeneration) return
    toast('error', error instanceof Error ? error.message : String(error))
    state.diffTarget = null
  } finally {
    if (generation === diffGeneration) state.diffLoading = false
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

interface TimestampPreviewRequest {
  timestamp: number
  description: string
  path?: string
  paths?: string[]
}

async function previewTimestamp({
  timestamp,
  description,
  path = '.',
  paths,
}: TimestampPreviewRequest) {
  if (state.projectId === null) return
  state.recoveryBusy = true
  state.applyResult = null
  try {
    state.recovery = await api<RecoveryView>(
      `/projects/${state.projectId}/recoveries`,
      {
        method: 'POST',
        body: JSON.stringify({
          timestamp,
          path,
          paths,
          request: description,
        }),
      },
    )
  } catch (error) {
    toast('error', error instanceof Error ? error.message : String(error))
  } finally {
    state.recoveryBusy = false
  }
}

function clearFocus() {
  state.focusId = null
  cancelLivePreview()
  scheduleRestorePreview()
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
  await previewTimestamp({
    timestamp: alert.target_timestamp,
    description: `Restore the project to just before the ${fmtClock(alert.started_at)} changes`,
  })
}

function dismissAlert(itemId: string) {
  state.dismissedAlerts.add(itemId)
  state.dismissedAlerts = new Set(state.dismissedAlerts)
}

async function applyRecovery(paths?: string[]) {
  if (!state.recovery) return
  if (paths && paths.length === 0) return
  state.recoveryBusy = true
  try {
    const result = await api<ApplyResult>(
      `/projects/${state.projectId}/recoveries/${state.recovery.id}/apply`,
      {
        method: 'POST',
        body: JSON.stringify(paths === undefined ? {} : { paths }),
      },
    )
    state.applyResult = result
    toast(
      'ok',
      result.already_applied
        ? 'This recovery plan was already applied'
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

function setRestoreTimestamp(timestamp: number | null) {
  if (state.restoreTimestamp === timestamp) return
  state.restoreTimestamp = timestamp
  scheduleRestorePreview()
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
    previewTimestamp,
    setRestoreTimestamp,
    fileRestoreAction,
    visibleTimelineItems,
    previewRestoreBefore,
    clearFocus,
    dismissAlert,
    applyRecovery,
    dismissRecovery,
    toast,
  }
}
