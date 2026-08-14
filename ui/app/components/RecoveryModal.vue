<script setup lang="ts">
import { computed, ref, watch } from 'vue'

const { state, applyRecovery, dismissRecovery } = useUndo()

const recovery = computed(() => state.recovery)
const selectedPaths = ref<Set<string>>(new Set())

watch(
  () => recovery.value?.id,
  () => {
    selectedPaths.value = new Set(
      recovery.value?.entries.map((entry) => entry.path) ?? [],
    )
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
    <div
      v-if="recovery"
      class="fixed inset-0 z-50 grid place-items-center bg-black/60 p-6 backdrop-blur-sm"
      @click.self="dismissRecovery"
    >
      <div
        class="rise-in flex max-h-[80vh] w-full max-w-xl flex-col overflow-hidden rounded-2xl border border-edge bg-panel shadow-2xl shadow-black/60"
      >
        <!-- Header -->
        <div class="flex items-center gap-3 border-b border-edge px-5 py-4">
          <span class="grid size-9 place-items-center rounded-xl border border-edge bg-well text-ink">
            <UiIcon name="undo" :size="16" />
          </span>
          <div class="min-w-0 flex-1">
            <h2 class="text-[14px] font-semibold text-ink">Review recovery plan</h2>
            <p class="truncate text-[12px] text-dim">
              {{ recovery.request }}
            </p>
          </div>
          <button
            class="grid size-7 place-items-center rounded-lg text-dim transition-colors hover:bg-well hover:text-ink"
            @click="dismissRecovery"
          >
            <UiIcon name="x" :size="14" />
          </button>
        </div>

        <!-- Summary -->
        <div class="flex gap-4 border-b border-edge bg-well/50 px-5 py-3 text-[12px]">
          <span class="text-mut">
            <span class="font-semibold text-ink">{{ selectedWrites }}</span>
            restore{{ selectedWrites === 1 ? '' : 's' }} selected
          </span>
          <span class="text-mut">
            <span class="font-semibold" :class="selectedDeletes > 0 ? 'text-del' : 'text-ink'">{{
              selectedDeletes
            }}</span>
            delete{{ selectedDeletes === 1 ? '' : 's' }} selected
          </span>
          <span class="flex-1" />
          <span class="text-dim">{{ confidenceLabel[recovery.confidence] ?? recovery.confidence }}</span>
        </div>

        <!-- Ambiguity warning -->
        <div
          v-if="recovery.ambiguity"
          class="flex items-start gap-2.5 border-b border-warn/20 bg-warn/8 px-5 py-3"
        >
          <UiIcon name="warning" :size="14" class="mt-0.5 shrink-0 text-warn" />
          <p class="text-[12px] leading-relaxed text-warn">
            Overlapping changes need review — this plan will not be applied:
            {{ recovery.ambiguity }}
          </p>
        </div>

        <!-- Plan entries -->
        <div class="flex items-center gap-3 border-b border-edge px-5 py-2 text-[11.5px]">
          <button
            class="font-medium text-mut transition-colors hover:text-ink"
            @click="selectAll"
          >
            Select all
          </button>
          <button
            class="font-medium text-mut transition-colors hover:text-ink"
            @click="clearAll"
          >
            Clear all
          </button>
          <span class="text-dim">
            {{ selectedEntries.length }} of {{ recovery.entries.length }} selected
          </span>
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto px-3 py-2">
          <p
            v-if="recovery.entries.length === 0"
            class="px-3 py-6 text-center text-[12.5px] text-dim"
          >
            This plan would not change any files — everything already matches the
            target state.
          </p>
          <div
            v-for="entry in recovery.entries"
            :key="entry.path"
            class="flex items-center gap-2.5 rounded-lg px-2.5 py-1.75 hover:bg-well/70"
          >
            <button
              class="grid size-3.75 shrink-0 place-items-center rounded border transition-colors"
              :class="
                selectedPaths.has(entry.path)
                  ? 'border-ink bg-ink text-bg'
                  : 'border-edge-strong bg-transparent text-transparent hover:border-dim'
              "
              :title="selectedPaths.has(entry.path) ? 'Exclude from restore' : 'Include in restore'"
              @click="togglePath(entry.path)"
            >
              <UiIcon name="check" :size="10" />
            </button>
            <span
              class="grid size-6 shrink-0 place-items-center rounded-md border border-edge bg-well"
              :class="entry.action === 'DELETE' ? 'text-del' : 'text-add'"
            >
              <UiIcon :name="entry.action === 'DELETE' ? 'trash' : 'undo'" :size="11" />
            </span>
            <span class="min-w-0 flex-1 truncate font-mono text-[12px] text-ink">{{
              entry.path
            }}</span>
            <span class="shrink-0 text-[11px] text-dim">
              {{
                entry.action === 'DELETE'
                  ? 'delete — file did not exist at the restore point'
                  : 'restore saved version'
              }}
            </span>
          </div>
        </div>

        <!-- Footer -->
        <div class="flex items-center gap-3 border-t border-edge bg-well/50 px-5 py-3.5">
          <p class="flex-1 text-[11.5px] leading-relaxed text-dim">
            Only selected files will be changed. Undo backs up their current
            versions first, so this restore can itself be undone.
          </p>
          <button
            class="rounded-lg border border-edge px-4 py-2 text-[12.5px] font-medium text-mut transition-colors hover:border-edge-strong hover:text-ink"
            @click="dismissRecovery"
          >
            Cancel
          </button>
          <button
            class="rounded-lg bg-ink px-4 py-2 text-[12.5px] font-semibold text-bg transition-opacity hover:opacity-85 disabled:cursor-not-allowed disabled:opacity-40"
            :disabled="!canApply"
            @click="applySelected"
          >
            {{ state.recoveryBusy ? 'Applying…' : `Apply selected · ${selectedEntries.length} file${selectedEntries.length === 1 ? '' : 's'}` }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>
