<script setup lang="ts">
import { computed } from 'vue'
import type { TimelineItem } from '~/types'
import { fmtClock, fmtDuration } from '~/utils/format'

const props = defineProps<{ item: TimelineItem }>()

const { state, toggleExpanded, toggleFile, setSelection, previewUndo, openDiff } =
  useUndo()

const expanded = computed(() => state.expanded.has(props.item.id))
const selection = computed(
  () => state.selections.get(props.item.id) ?? new Set<string>(),
)
const selectedCount = computed(() => selection.value.size)
const allPaths = computed(() => props.item.files.map((file) => file.path))
const isActive = computed(() => props.item.status === 'active')
const destructive = computed(() => props.item.deleted_files >= 3)

const statusChip = computed(() => {
  switch (props.item.status) {
    case 'active':
      return { text: 'Active', class: 'border-accent/30 bg-accent/10 text-accent' }
    case 'failed':
      return { text: 'Failed', class: 'border-del/30 bg-del/10 text-del' }
    case 'aborted':
      return { text: 'Aborted', class: 'border-warn/30 bg-warn/10 text-warn' }
    default:
      return null
  }
})

function undoSelected() {
  void previewUndo({
    item: props.item,
    paths: [...selection.value],
    description: `Undo ${selectedCount.value} selected file${selectedCount.value === 1 ? '' : 's'} from ${props.item.label}`,
  })
}

function keepSelectedUndoRest() {
  const rest = allPaths.value.filter((path) => !selection.value.has(path))
  void previewUndo({
    item: props.item,
    paths: rest,
    description: `Keep ${selectedCount.value} file${selectedCount.value === 1 ? '' : 's'}, undo the other ${rest.length} from ${props.item.label}`,
  })
}

function undoEverything() {
  void previewUndo({
    item: props.item,
    paths: allPaths.value,
    description: `Undo everything from ${props.item.label} (${allPaths.value.length} files)`,
  })
}

function toggleSelectAll() {
  setSelection(
    props.item.id,
    selectedCount.value === allPaths.value.length ? [] : allPaths.value,
  )
}
</script>

<template>
  <article
    :id="`item-${props.item.id}`"
    class="rise-in overflow-hidden rounded-xl border bg-panel transition-colors"
    :class="expanded ? 'border-edge-strong' : 'border-edge hover:border-edge-strong'"
  >
    <!-- Card header -->
    <button
      class="flex w-full items-center gap-3 px-4 py-3 text-left"
      @click="toggleExpanded(props.item.id)"
    >
      <!-- Machine-paced un-attributed groups get the bolt: Undo saw
           tool-speed changes but cannot name the process behind them. -->
      <span
        class="grid size-8 shrink-0 place-items-center rounded-lg border border-edge bg-well"
        :class="props.item.kind === 'run' ? 'text-accent' : 'text-mut'"
      >
        <UiIcon
          :name="
            props.item.kind === 'run'
              ? props.item.actor === 'tool'
                ? 'terminal'
                : 'bot'
              : props.item.pace === 'machine'
                ? 'zap'
                : 'pencil'
          "
          :size="15"
        />
      </span>

      <span class="min-w-0 flex-1">
        <span class="flex items-center gap-2">
          <span class="truncate text-[13.5px] font-semibold text-ink">
            {{ props.item.label }}
          </span>
          <span
            v-if="props.item.scope_hint"
            class="shrink-0 truncate font-mono text-[11px] text-dim"
            :title="`Most of these files live under ${props.item.scope_hint}/`"
          >
            {{ props.item.scope_hint }}
          </span>
          <span
            v-if="props.item.run_id"
            class="shrink-0 rounded bg-well px-1.5 py-px font-mono text-[10px] text-dim"
          >
            {{ props.item.run_id }}
          </span>
          <span
            v-if="statusChip"
            class="shrink-0 rounded-full border px-2 py-px text-[10.5px] font-medium"
            :class="statusChip.class"
          >
            <span v-if="isActive" class="mr-1 inline-block size-1 animate-pulse rounded-full bg-accent align-middle" />{{ statusChip.text }}
          </span>
          <span
            v-if="destructive"
            class="flex shrink-0 items-center gap-1 rounded-full border border-del/30 bg-del/10 px-2 py-px text-[10.5px] font-medium text-del"
            :title="`${props.item.deleted_files} files deleted`"
          >
            <UiIcon name="warning" :size="10" />
            {{ props.item.deleted_files }} deleted
          </span>
        </span>
        <span class="mt-0.5 flex items-center gap-1.5 text-[11.5px] text-dim">
          <span class="font-mono">{{ fmtClock(props.item.started_at) }}</span>
          <span>·</span>
          <span>{{ fmtDuration(props.item.started_at, props.item.ended_at, state.timeline?.now ?? props.item.started_at) }}</span>
          <span>·</span>
          <span
            >{{ props.item.file_count }} file{{ props.item.file_count === 1 ? '' : 's' }}
            changed</span
          >
          <template v-if="props.item.intent">
            <span>·</span>
            <span class="truncate italic text-mut">“{{ props.item.intent }}”</span>
          </template>
        </span>
      </span>

      <span class="flex shrink-0 items-center gap-2 font-mono text-[11.5px]">
        <span class="text-add">+{{ props.item.inserted }}</span>
        <span class="text-del">−{{ props.item.deleted }}</span>
      </span>
      <UiIcon
        name="chevron"
        :size="14"
        class="shrink-0 text-dim transition-transform duration-200"
        :class="expanded ? 'rotate-180' : ''"
      />
    </button>

    <!-- Expanded body -->
    <div v-if="expanded" class="border-t border-edge">
      <div class="flex items-center gap-3 px-4 pb-1 pt-2.5">
        <button
          class="text-[11.5px] font-medium text-dim transition-colors hover:text-ink"
          @click="toggleSelectAll"
        >
          {{ selectedCount === allPaths.length ? 'Clear selection' : 'Select all' }}
        </button>
        <span v-if="selectedCount > 0" class="text-[11.5px] text-mut">
          {{ selectedCount }} of {{ allPaths.length }} selected
        </span>
        <span v-if="props.item.stats_truncated" class="text-[11px] text-dim">
          line counts computed for the first 500 files
        </span>
      </div>

      <div class="max-h-80 overflow-y-auto px-2 pb-2">
        <FileRow
          v-for="file in props.item.files"
          :key="file.path"
          :file="file"
          :item-id="props.item.id"
          :selected="selection.has(file.path)"
          :active="
            state.diffTarget?.itemId === props.item.id &&
            state.diffTarget?.file.path === file.path
          "
          @toggle="toggleFile(props.item.id, file.path)"
          @open="openDiff(props.item.id, file)"
        />
      </div>

      <!-- Checkpoints recorded during this run -->
      <div
        v-if="props.item.checkpoints.length > 0"
        class="flex flex-wrap gap-2 border-t border-edge px-4 py-2"
      >
        <span
          v-for="checkpoint in props.item.checkpoints"
          :key="checkpoint.id"
          class="flex items-center gap-1.5 rounded-full border border-edge bg-well px-2.5 py-0.5 text-[11px] text-mut"
        >
          <UiIcon name="flag" :size="10" class="text-dim" />
          {{ checkpoint.name }}
        </span>
      </div>

      <!-- Actions -->
      <div class="flex items-center gap-2 border-t border-edge bg-well/50 px-4 py-2.5">
        <template v-if="isActive">
          <span class="text-[12px] text-dim">
            This Run is still active — undo becomes available when it finishes.
          </span>
        </template>
        <template v-else>
          <button
            v-if="selectedCount > 0"
            class="rounded-lg bg-ink px-3.5 py-1.5 text-[12.5px] font-semibold text-bg transition-opacity hover:opacity-85 disabled:opacity-40"
            :disabled="state.recoveryBusy"
            @click="undoSelected"
          >
            Undo {{ selectedCount }} selected
          </button>
          <button
            v-if="selectedCount > 0 && selectedCount < allPaths.length"
            class="rounded-lg border border-edge px-3.5 py-1.5 text-[12.5px] font-medium text-mut transition-colors hover:border-edge-strong hover:text-ink disabled:opacity-40"
            :disabled="state.recoveryBusy"
            @click="keepSelectedUndoRest"
          >
            Keep selected, undo the rest
          </button>
          <button
            v-if="selectedCount === 0"
            class="rounded-lg border border-edge px-3.5 py-1.5 text-[12.5px] font-medium text-mut transition-colors hover:border-del/40 hover:text-del disabled:opacity-40"
            :disabled="state.recoveryBusy"
            @click="undoEverything"
          >
            <span class="flex items-center gap-1.5">
              <UiIcon name="undo" :size="12" />
              Undo everything from this {{ props.item.kind === 'run' ? 'run' : 'group' }}
            </span>
          </button>
          <span class="flex-1" />
          <span class="text-[11px] text-dim">Nothing changes until you review a plan</span>
        </template>
      </div>
    </div>
  </article>
</template>
