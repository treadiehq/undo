<script setup lang="ts">
import { computed } from 'vue'
import type { TimelineItem } from '~/types'
import { fmtClock, fmtDuration } from '~/utils/format'

const props = defineProps<{
  item: TimelineItem
  restoreImpact?: 'undo' | 'partial' | 'keep' | null
}>()

const { state, toggleExpanded, toggleFile, setSelection, previewUndo, openDiff } =
  useUndo()

const expanded = computed(() => state.expanded.has(props.item.id))
const selection = computed(
  () => state.selections.get(props.item.id) ?? new Set<string>(),
)
const selectedCount = computed(() => selection.value.size)
const allPaths = computed(() =>
  props.item.files.filter((file) => file.recoverable).map((file) => file.path),
)
const isActive = computed(() => props.item.status === 'active')
const destructive = computed(() => props.item.deleted_files >= 3)
const blockedCount = computed(
  () => props.item.files.filter((file) => !file.recoverable).length,
)
const impactChip = computed(() => {
  switch (props.restoreImpact) {
    case 'undo':
      return {
        text: 'Will undo',
        class: 'border-accent/30 bg-accent/10 text-accent',
        description:
          'This activity happened after the restore point and is expected to be reverted.',
      }
    case 'partial':
      return {
        text: 'Partially undo',
        class: 'border-warn/30 bg-warn/10 text-warn',
        description:
          'This activity crosses the restore point. Only its later changes are expected to be reverted.',
      }
    case 'keep':
      return {
        text: 'Will keep',
        class: 'border-edge bg-well text-dim',
        description:
          'This activity finished before the restore point and is expected to remain.',
      }
    default:
      return null
  }
})

const statusChip = computed(() => {
  switch (props.item.status) {
    case 'active':
      return { text: 'Active', class: 'border-accent/30 bg-accent/10 text-accent' }
    case 'failed':
      return { text: 'Failed', class: 'border-del/30 bg-del/10 text-del' }
    case 'aborted':
      return { text: 'Aborted', class: 'border-warn/30 bg-warn/10 text-warn' }
    case 'blocked':
      return { text: 'Blocked', class: 'border-warn/30 bg-warn/10 text-warn' }
    default:
      return null
  }
})

function previewSelectedRestore() {
  void previewUndo({
    item: props.item,
    paths: [...selection.value],
    description: `Restore ${selectedCount.value} selected file${selectedCount.value === 1 ? '' : 's'} to before ${props.item.label}`,
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
    :class="[
      expanded ? 'border-edge-strong' : 'border-edge hover:border-edge-strong',
      props.restoreImpact === 'undo'
        ? 'ring-1 ring-inset ring-accent/60'
        : props.restoreImpact === 'partial'
          ? 'ring-1 ring-inset ring-warn/50'
          : props.restoreImpact === 'keep'
            ? 'opacity-55'
            : '',
    ]"
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
        :class="
          props.item.kind === 'collision'
            ? 'text-warn'
            : props.item.kind === 'run'
              ? 'text-accent'
              : 'text-mut'
        "
      >
        <UiIcon
          :name="
            props.item.kind === 'collision'
              ? 'warning'
              : props.item.kind === 'run'
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
            v-if="statusChip"
            class="shrink-0 rounded-full border px-2 py-px text-[10.5px] font-medium"
            :class="statusChip.class"
          >
            <span v-if="isActive" class="mr-1 inline-block size-1 animate-pulse rounded-full bg-accent align-middle" />{{ statusChip.text }}
          </span>
          <span
            v-if="impactChip"
            class="shrink-0 rounded-full border px-2 py-px text-[10.5px] font-medium"
            :class="impactChip.class"
            :title="impactChip.description"
          >
            {{ impactChip.text }}
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
          v-if="allPaths.length > 0"
          class="text-[11.5px] font-medium text-dim transition-colors hover:text-ink"
          @click="toggleSelectAll"
        >
          {{ selectedCount === allPaths.length ? 'Clear selection' : 'Select all' }}
        </button>
        <span v-if="selectedCount > 0" class="text-[11.5px] text-mut">
          {{ selectedCount }} of {{ allPaths.length }} selected
        </span>
        <span v-else-if="allPaths.length === 0" class="text-[11.5px] text-warn">
          No whole-file recovery available
        </span>
        <span v-if="props.item.stats_truncated" class="text-[11px] text-dim">
          line counts computed for the first 500 files
        </span>
      </div>

      <div
        v-if="blockedCount > 0"
        class="mx-4 mb-2 flex items-start gap-2 rounded-lg border border-warn/20 bg-warn/8 px-3 py-2 text-[11.5px] leading-relaxed text-warn"
      >
        <UiIcon name="warning" :size="12" class="mt-0.5 shrink-0" />
        <span>
          {{ blockedCount }} file{{ blockedCount === 1 ? '' : 's' }} cannot be
          restored as whole files because ownership is collision, interleaved,
          or unattributed. Open a file to inspect the recorded diff.
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
            This Run is still active — restore previews become available when it finishes.
          </span>
        </template>
        <template v-else-if="allPaths.length > 0">
          <button
            class="rounded-lg bg-ink px-3.5 py-1.5 text-[12.5px] font-semibold text-bg transition-opacity hover:opacity-85 disabled:opacity-40"
            :disabled="selectedCount === 0 || state.recoveryBusy"
            @click="previewSelectedRestore"
          >
            Preview restore for {{ selectedCount }} file{{ selectedCount === 1 ? '' : 's' }}
          </button>
          <span class="flex-1" />
          <span class="text-[11px] text-dim">
            Select files first; nothing changes until you apply the reviewed plan
          </span>
        </template>
        <template v-else>
          <span class="text-[12px] text-warn">
            Recovery is disabled because no file has exclusive reported ownership.
          </span>
        </template>
      </div>
    </div>
  </article>
</template>
