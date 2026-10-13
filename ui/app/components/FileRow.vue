<script setup lang="ts">
import type { FileChange } from '~/types'
import { splitPath } from '~/utils/format'

const props = defineProps<{
  file: FileChange
  itemId: string
  selected: boolean
  active: boolean
}>()

const emit = defineEmits<{ toggle: []; open: [] }>()
</script>

<template>
  <div
    class="relative flex min-h-12 items-center gap-3 px-5 py-2 transition-colors"
    :class="active ? 'bg-selected before:absolute before:inset-y-0 before:left-0 before:w-0.5 before:bg-accent' : 'hover:bg-edge/40'"
  >
    <input
      type="checkbox"
      class="ui-checkbox"
      :checked="selected"
      :disabled="!props.file.recoverable"
      :aria-label="`Include ${props.file.path} in run restore`"
      :title="props.file.warning ?? undefined"
      @change="emit('toggle')"
    />
    <button
      class="flex min-w-0 flex-1 items-center gap-3 text-left"
      :aria-label="`View changes to ${props.file.path}`"
      :aria-pressed="active"
      :title="`${props.file.path} · ${props.file.event_count} recorded change${props.file.event_count === 1 ? '' : 's'}`"
      @click="emit('open')"
    >
      <span class="min-w-0 flex-1">
        <span class="block truncate text-[12.5px] font-medium text-ink">{{ splitPath(props.file.path).name }}</span>
        <span class="block truncate text-[11px] text-mut">
          <template v-if="props.file.old_path">{{ props.file.old_path }} → </template>
          {{ splitPath(props.file.path).dir.replace(/\/$/, '') || 'Project root' }}
        </span>
        <span v-if="!props.file.recoverable" class="block text-[11px] text-warn" :title="props.file.warning ?? undefined">
          {{ props.file.ownership_status }} · whole-file restore unavailable
        </span>
      </span>
      <span class="flex shrink-0 flex-col items-end gap-0.5 text-[11px]">
        <span v-if="props.file.change !== 'modified'" class="capitalize" :class="props.file.change === 'deleted' ? 'text-del' : 'text-mut'">
          {{ props.file.change }}
        </span>
        <span v-if="props.file.binary" class="text-mut">Binary</span>
        <span v-else class="flex gap-2 font-mono tabular-nums">
          <span class="text-add">+{{ props.file.inserted }}</span>
          <span class="text-del">−{{ props.file.deleted }}</span>
        </span>
      </span>
    </button>
  </div>
</template>
