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

const changeColor: Record<string, string> = {
  created: 'text-add',
  deleted: 'text-del',
  modified: 'text-warn',
  renamed: 'text-accent',
}

const changeGlyph: Record<string, string> = {
  created: 'A',
  deleted: 'D',
  modified: 'M',
  renamed: 'R',
}

// GitHub-style five-block churn bar.
function blocks(file: FileChange): Array<'add' | 'del' | 'none'> {
  const total = file.inserted + file.deleted
  if (total === 0) return ['none', 'none', 'none', 'none', 'none']
  const adds = Math.round((file.inserted / total) * 5)
  return Array.from({ length: 5 }, (_, index) =>
    index < adds ? 'add' : 'del',
  )
}
</script>

<template>
  <div
    class="group flex cursor-pointer items-center gap-2.5 rounded-lg px-2.5 py-[7px] transition-colors"
    :class="active ? 'bg-well ring-1 ring-edge-strong' : 'hover:bg-well/70'"
    @click="emit('open')"
  >
    <!-- Checkbox: includes the file in a restore preview -->
    <button
      class="grid size-[15px] shrink-0 place-items-center rounded border transition-colors"
      :class="
        !props.file.recoverable
          ? 'cursor-not-allowed border-edge bg-well text-transparent opacity-45'
          : selected
          ? 'border-ink bg-ink text-bg'
          : 'border-edge-strong bg-transparent text-transparent group-hover:border-dim'
      "
      :disabled="!props.file.recoverable"
      :title="
        props.file.warning ??
        (selected ? 'Remove from restore' : 'Select to restore')
      "
      @click.stop="emit('toggle')"
    >
      <UiIcon name="check" :size="10" />
    </button>

    <span
      class="w-3 shrink-0 text-center font-mono text-[11px] font-bold"
      :class="changeColor[props.file.change]"
      :title="props.file.change"
    >
      {{ changeGlyph[props.file.change] }}
    </span>

    <span class="min-w-0 flex-1 truncate font-mono text-[12.5px]">
      <template v-if="props.file.old_path">
        <span class="text-dim">{{ props.file.old_path }}</span>
        <span class="mx-1 text-dim">→</span>
      </template>
      <span class="text-dim">{{ splitPath(props.file.path).dir }}</span
      ><span class="text-ink">{{ splitPath(props.file.path).name }}</span>
    </span>

    <span
      v-if="!props.file.recoverable"
      class="shrink-0 rounded border border-warn/25 bg-warn/8 px-1.5 py-px text-[10px] text-warn"
      :title="props.file.warning ?? undefined"
    >
      {{ props.file.ownership_status }}
    </span>

    <span
      v-if="props.file.event_count > 1"
      class="shrink-0 rounded bg-well px-1.5 py-px font-mono text-[10px] text-dim"
      :title="`${props.file.event_count} recorded changes`"
    >
      ×{{ props.file.event_count }}
    </span>

    <span v-if="props.file.binary" class="shrink-0 font-mono text-[10px] uppercase text-dim">
      binary
    </span>
    <template v-else>
      <span class="shrink-0 font-mono text-[11px] text-add">+{{ props.file.inserted }}</span>
      <span class="shrink-0 font-mono text-[11px] text-del">−{{ props.file.deleted }}</span>
      <span class="flex shrink-0 gap-[2px]">
        <span
          v-for="(block, index) in blocks(props.file)"
          :key="index"
          class="size-[7px] rounded-[2px]"
          :class="{
            'bg-add/70': block === 'add',
            'bg-del/70': block === 'del',
            'bg-edge': block === 'none',
          }"
        />
      </span>
    </template>
  </div>
</template>
