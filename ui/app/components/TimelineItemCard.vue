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
        class: 'text-accent',
        description:
          'This activity happened after the restore point and is expected to be reverted.',
      }
    case 'partial':
      return {
        text: 'Later changes only',
        class: 'text-mut',
        description:
          'This activity crosses the restore point. Only its later changes are expected to be reverted.',
      }
    case 'keep':
      return {
        text: 'Will keep',
        class: 'text-mut',
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
      return { text: 'Recording', class: 'text-accent' }
    case 'failed':
      return { text: 'Failed', class: 'text-del' }
    case 'aborted':
      return { text: 'Aborted', class: 'text-warn' }
    case 'blocked':
      return { text: 'Blocked', class: 'text-warn' }
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
  <article :id="`item-${props.item.id}`" class="border-b border-edge">
    <button
      class="flex w-full items-start gap-3 px-5 py-4 text-left hover:bg-edge/25"
      :aria-expanded="expanded"
      :aria-controls="`files-${props.item.id}`"
      @click="toggleExpanded(props.item.id)"
    >
      <UiIcon
        :name="props.item.kind === 'collision' ? 'warning' : props.item.kind === 'run' ? props.item.actor === 'tool' ? 'terminal' : 'bot' : props.item.pace === 'machine' ? 'zap' : 'pencil'"
        :size="17"
        class="mt-0.5 shrink-0"
        :class="props.item.kind === 'collision' ? 'text-warn' : 'text-mut'"
      />
      <span class="min-w-0 flex-1">
        <span class="flex flex-wrap items-center gap-x-2.5 gap-y-1">
          <span class="min-w-0 break-words text-[14px] font-medium text-ink">{{ props.item.label }}</span>
          <span v-if="statusChip" class="flex items-center gap-1.5 text-[11px]" :class="statusChip.class">
            <span v-if="isActive" class="size-1 rounded-full bg-accent" />{{ statusChip.text }}
          </span>
        </span>
        <span class="mt-1 flex flex-wrap items-center gap-x-1.5 text-[11.5px] text-mut">
          <span class="tabular-nums">{{ fmtClock(props.item.started_at) }} · {{ fmtDuration(props.item.started_at, props.item.ended_at, state.timeline?.now ?? props.item.started_at) }}</span>
          <span>·</span>
          <span>{{ props.item.file_count }} file{{ props.item.file_count === 1 ? '' : 's' }}</span>
        </span>
        <span v-if="props.item.intent" class="mt-1 block text-[12px] text-mut">{{ props.item.intent }}</span>
        <span v-else-if="props.item.scope_hint" class="mt-1 block truncate text-[11px] text-mut">{{ props.item.scope_hint }}/</span>
        <span class="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px]">
          <span v-if="impactChip" :class="impactChip.class" :title="impactChip.description">{{ impactChip.text }}</span>
          <span v-if="destructive" class="flex items-center gap-1 text-del"><UiIcon name="warning" :size="11" />{{ props.item.deleted_files }} deleted</span>
        </span>
      </span>
      <span class="mt-0.5 flex shrink-0 items-center gap-2 font-mono text-[11px] tabular-nums">
        <span class="text-add">+{{ props.item.inserted }}</span>
        <span class="text-del">−{{ props.item.deleted }}</span>
      </span>
      <UiIcon name="chevron" :size="12" class="mt-1 shrink-0 text-dim" :class="expanded ? 'rotate-180' : ''" />
    </button>

    <div v-if="expanded" :id="`files-${props.item.id}`">
      <div class="flex min-h-9 flex-wrap items-center justify-between gap-2 border-y border-edge px-5 py-2 text-[11px] text-mut">
        <label v-if="allPaths.length > 0" class="flex items-center gap-3">
          <input
            type="checkbox"
            class="ui-checkbox"
            :checked="selectedCount === allPaths.length"
            :indeterminate="selectedCount > 0 && selectedCount < allPaths.length"
            :aria-label="`Select all recoverable files in ${props.item.label}`"
            @change="toggleSelectAll"
          />
          Select all
        </label>
        <span v-if="allPaths.length > 0">{{ selectedCount }} selected</span>
        <span v-else class="text-warn">No whole-file restore available</span>
        <span v-if="props.item.stats_truncated" class="w-full">Line counts cover the first 500 files.</span>
      </div>

      <div v-if="blockedCount > 0" class="flex items-start gap-2 px-5 py-3 text-[12px] leading-relaxed text-warn">
        <UiIcon name="warning" :size="13" class="mt-0.5 shrink-0" />
        <span>{{ blockedCount }} file{{ blockedCount === 1 ? '' : 's' }} cannot be restored as whole files because ownership is shared or unknown. Open a file to review its changes.</span>
      </div>

      <div class="max-h-[min(32rem,55vh)] overflow-y-auto">
        <FileRow
          v-for="file in props.item.files"
          :key="file.path"
          :file="file"
          :item-id="props.item.id"
          :selected="selection.has(file.path)"
          :active="state.diffTarget?.itemId === props.item.id && state.diffTarget?.file.path === file.path"
          @toggle="toggleFile(props.item.id, file.path)"
          @open="openDiff(props.item.id, file)"
        />
      </div>

      <div v-if="props.item.checkpoints.length > 0" class="flex flex-wrap gap-x-4 gap-y-2 border-t border-edge px-5 py-3 text-[11px] text-mut">
        <span v-for="checkpoint in props.item.checkpoints" :key="checkpoint.id" class="flex items-center gap-1.5"><UiIcon name="flag" :size="11" />{{ checkpoint.name }}</span>
      </div>

      <div class="border-t border-edge px-5 py-3">
        <p v-if="isActive" class="text-[12px] leading-relaxed text-mut">
          This run is still recording. Restoring selected run files becomes available when it finishes.
        </p>
        <template v-else-if="allPaths.length > 0">
          <button class="ui-button w-full" :disabled="selectedCount === 0 || state.recoveryBusy" @click="previewSelectedRestore">
            Preview {{ selectedCount }} selected file{{ selectedCount === 1 ? '' : 's' }}
          </button>
          <p class="mt-2 text-[11px] text-mut">Restore selected files to before this activity.</p>
        </template>
        <p v-else class="text-[12px] leading-relaxed text-warn">Whole-file restore requires exclusive recorded ownership.</p>
      </div>
    </div>
  </article>
</template>
