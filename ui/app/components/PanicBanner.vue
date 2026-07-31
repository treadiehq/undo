<script setup lang="ts">
import { nextTick } from 'vue'
import { fmtAgo, fmtClock } from '~/utils/format'

// The "oh no" moment, in the UI: a recent un-attributed group deleted
// multiple files. One click previews a whole-project restore to just before
// it happened — the same preview-then-apply plan as `undo panic`.
const {
  state,
  panicAlert,
  previewRestoreBefore,
  dismissAlert,
  clearFocus,
  toggleExpanded,
} = useUndo()

async function showItem() {
  if (!panicAlert.value) return
  const id = panicAlert.value.item_id
  clearFocus()
  if (!state.expanded.has(id)) {
    toggleExpanded(id)
  }
  await nextTick()
  document
    .getElementById(`item-${id}`)
    ?.scrollIntoView({ behavior: 'smooth', block: 'center' })
}
</script>

<template>
  <div
    v-if="panicAlert"
    class="rise-in mb-3 rounded-xl border border-del/30 bg-del/10 px-4 py-3"
  >
    <div class="flex items-start gap-3">
      <span class="mt-0.5 grid size-8 shrink-0 place-items-center rounded-lg border border-del/30 bg-del/10 text-del">
        <UiIcon name="warning" :size="15" />
      </span>
      <div class="min-w-0 flex-1">
        <p class="text-[13px] font-semibold leading-snug text-ink">
          {{ panicAlert.deleted_files }} file{{ panicAlert.deleted_files === 1 ? '' : 's' }}
          deleted by unattributed changes
          {{ fmtAgo(panicAlert.started_at, state.timeline?.now ?? panicAlert.started_at) }}
        </p>
        <p class="mt-0.5 text-[12px] leading-relaxed text-mut">
          {{ panicAlert.file_count }} file{{ panicAlert.file_count === 1 ? '' : 's' }}
          touched around {{ fmtClock(panicAlert.started_at) }}. Every version is
          still recorded — nothing is lost yet.
        </p>
      </div>
      <button
        class="shrink-0 p-1 text-dim transition-colors hover:text-ink"
        title="Dismiss"
        @click="dismissAlert(panicAlert.item_id)"
      >
        <UiIcon name="x" :size="13" />
      </button>
    </div>
    <div class="mt-2.5 flex items-center gap-2 pl-11">
      <button
        class="rounded-lg bg-del px-3.5 py-1.5 text-[12.5px] font-semibold text-bg transition-opacity hover:opacity-85 disabled:opacity-40"
        :disabled="state.recoveryBusy"
        @click="previewRestoreBefore(panicAlert)"
      >
        Preview restore to before
      </button>
      <button
        class="rounded-lg border border-edge px-3 py-1.5 text-[12px] font-medium text-mut transition-colors hover:border-edge-strong hover:text-ink"
        @click="showItem"
      >
        Show me
      </button>
    </div>
  </div>
</template>
