<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { splitPath } from '~/utils/format'

const { state, applyRecovery, dismissRecovery } = useUndo()

const recovery = computed(() => state.recovery)
const dialog = ref<HTMLDialogElement | null>(null)
const selectedPaths = ref<Set<string>>(new Set())
let returnFocus: HTMLElement | null = null

watch(
  () => state.recoveryBusy,
  (busy) => {
    if (busy && !recovery.value && typeof document !== 'undefined') {
      returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null
    }
  },
)

watch(
  () => recovery.value?.id,
  (id, previousId) => {
    if (id && !previousId && !returnFocus && typeof document !== 'undefined') {
      returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null
    }
    selectedPaths.value = new Set(
      recovery.value?.entries.map((entry) => entry.path) ?? [],
    )
    void nextTick(() => {
      if (recovery.value && dialog.value && !dialog.value.open) dialog.value.showModal()
      if (!recovery.value) {
        if (returnFocus?.isConnected) returnFocus.focus()
        returnFocus = null
      }
    })
  },
  { immediate: true },
)

const selectedEntries = computed(
  () =>
    recovery.value?.entries.filter((entry) => selectedPaths.value.has(entry.path)) ??
    [],
)
const selectedWrites = computed(
  () => selectedEntries.value.filter((entry) => entry.action === 'WRITE').length,
)
const selectedDeletes = computed(
  () => selectedEntries.value.filter((entry) => entry.action === 'DELETE').length,
)

const confidenceLabel: Record<string, string> = {
  'exact-paths': 'Exact — the files you selected',
  'exact-timestamp': 'Exact — a saved point in time',
  'explicit-intent': 'High — matched a completed task boundary',
  ambiguous: 'Needs review — changes overlap',
}

const canApply = computed(
  () =>
    !!recovery.value &&
    selectedEntries.value.length > 0 &&
    !recovery.value.ambiguity &&
    !state.recoveryBusy,
)

function togglePath(path: string) {
  const next = new Set(selectedPaths.value)
  if (next.has(path)) next.delete(path)
  else next.add(path)
  selectedPaths.value = next
}

function selectAll() {
  selectedPaths.value = new Set(
    recovery.value?.entries.map((entry) => entry.path) ?? [],
  )
}

function clearAll() {
  selectedPaths.value = new Set()
}

function applySelected() {
  void applyRecovery([...selectedPaths.value])
}
</script>

<template>
  <Teleport to="body">
    <dialog
      v-if="recovery"
      ref="dialog"
      class="recovery-dialog m-auto max-h-[85dvh] w-[calc(100%-2rem)] max-w-xl flex-col overflow-hidden rounded-xl border border-edge-strong bg-bg p-0 text-ink shadow-2xl shadow-black/40 open:flex"
      aria-labelledby="recovery-heading"
      aria-describedby="recovery-request"
      @cancel.prevent="!state.recoveryBusy && dismissRecovery()"
    >
      <div class="flex items-start gap-4 px-6 pt-6 pb-5">
        <div class="min-w-0 flex-1">
          <h2 id="recovery-heading" class="text-[19px] font-medium tracking-tight">Review restore</h2>
          <p id="recovery-request" class="mt-1.5 break-words text-[12px] leading-relaxed text-mut">{{ recovery.request }}</p>
        </div>
        <button class="rounded p-1 text-mut hover:text-ink disabled:opacity-40" aria-label="Close restore preview" :disabled="state.recoveryBusy" @click="dismissRecovery">
          <UiIcon name="x" :size="17" />
        </button>
      </div>

      <div class="flex flex-wrap gap-x-5 gap-y-1 border-y border-edge px-6 py-3 text-[12px] text-mut" :title="confidenceLabel[recovery.confidence] ?? recovery.confidence">
        <span><span class="font-medium text-ink">{{ selectedWrites }}</span> saved version{{ selectedWrites === 1 ? '' : 's' }} to restore</span>
        <span v-if="selectedDeletes > 0" class="text-del">{{ selectedDeletes }} file{{ selectedDeletes === 1 ? '' : 's' }} to delete</span>
      </div>

      <div v-if="recovery.ambiguity" class="flex items-start gap-2.5 border-b border-warn/20 bg-warn/8 px-6 py-3" role="alert">
        <UiIcon name="warning" :size="14" class="mt-0.5 shrink-0 text-warn" />
        <p class="text-[12px] leading-relaxed text-warn">Overlapping changes need review. This plan cannot be applied: {{ recovery.ambiguity }}</p>
      </div>

      <div class="flex flex-wrap items-center justify-between gap-3 px-6 py-3 text-[11px] text-mut">
        <label class="flex items-center gap-3">
          <input
            type="checkbox"
            class="ui-checkbox"
            aria-label="Select all files in restore preview"
            :checked="recovery.entries.length > 0 && selectedEntries.length === recovery.entries.length"
            :indeterminate="selectedEntries.length > 0 && selectedEntries.length < recovery.entries.length"
            :disabled="state.recoveryBusy || recovery.entries.length === 0"
            @change="selectedEntries.length === recovery.entries.length ? clearAll() : selectAll()"
          />
          Select all
        </label>
        <span>{{ selectedEntries.length }} of {{ recovery.entries.length }} selected</span>
      </div>
      <div class="min-h-0 flex-1 overflow-y-auto px-6 pb-3">
        <p v-if="recovery.entries.length === 0" class="py-6 text-[13px] leading-relaxed text-mut">Everything already matches the restore point. No files need to change.</p>
        <label v-for="entry in recovery.entries" :key="entry.path" class="flex items-start gap-3 border-t border-edge py-3">
          <input
            type="checkbox"
            class="ui-checkbox mt-1"
            :checked="selectedPaths.has(entry.path)"
            :disabled="state.recoveryBusy"
            :aria-label="`Include ${entry.path} in restore`"
            @change="togglePath(entry.path)"
          />
          <span class="min-w-0 flex-1">
            <span class="block break-words text-[12.5px] font-medium" :title="entry.path">{{ splitPath(entry.path).name }}</span>
            <span v-if="splitPath(entry.path).dir" class="block break-all text-[11px] text-mut">{{ splitPath(entry.path).dir }}</span>
            <span class="mt-0.5 block text-[11px]" :class="entry.action === 'DELETE' ? 'text-del' : 'text-mut'">
              {{ entry.action === 'DELETE' ? 'Delete · did not exist at the restore point' : 'Restore saved version' }}
            </span>
          </span>
        </label>
      </div>

      <div class="border-t border-edge px-6 py-5">
        <p class="text-[12px] leading-relaxed text-mut">Other files will stay as they are. Undo backs up the selected files first, so this restore can itself be undone.</p>
        <div class="mt-5 flex justify-end gap-2.5">
          <button class="ui-button" :disabled="state.recoveryBusy" @click="dismissRecovery">Cancel</button>
          <button class="ui-button ui-button-primary" :disabled="!canApply" @click="applySelected">
            {{ state.recoveryBusy ? 'Restoring…' : `Restore ${selectedEntries.length} file${selectedEntries.length === 1 ? '' : 's'}` }}
          </button>
        </div>
      </div>
    </dialog>
  </Teleport>
</template>

<style scoped>
.recovery-dialog::backdrop { background: color-mix(in srgb, var(--color-bg) 65%, transparent); }
</style>
