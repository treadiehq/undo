<script setup lang="ts">
import { computed } from 'vue'
import type { TimelineItem } from '~/types'
import { fmtDay } from '~/utils/format'

const { state, focusedItem, clearFocus } = useUndo()

// Items grouped under day separators (newest first, matching payload order).
// In focus mode (`undo ui r_421`) the feed shows only the reviewed Run; the
// full timeline is one click away.
const dayGroups = computed(() => {
  const source = focusedItem.value
    ? [focusedItem.value]
    : (state.timeline?.items ?? [])
  const groups: Array<{ day: string; items: TimelineItem[] }> = []
  for (const item of source) {
    const day = fmtDay(item.started_at)
    const last = groups[groups.length - 1]
    if (last && last.day === day) last.items.push(item)
    else groups.push({ day, items: [item] })
  }
  return groups
})
</script>

<template>
  <section class="min-h-0 overflow-y-auto px-5 py-4">
    <!-- Panic alert: a recent un-attributed group deleted multiple files -->
    <PanicBanner v-if="!state.timelineLoading" />

    <!-- Focus mode banner -->
    <div
      v-if="focusedItem"
      class="mb-3 flex items-center gap-2.5 rounded-xl border border-accent/25 bg-accent/5 px-4 py-2.5"
    >
      <UiIcon name="bot" :size="13" class="shrink-0 text-accent" />
      <span class="min-w-0 flex-1 truncate text-[12.5px] text-mut">
        Reviewing
        <span class="font-semibold text-ink">{{ focusedItem.label }}</span>
        <span class="ml-1.5 font-mono text-[11px] text-dim">{{ focusedItem.id }}</span>
      </span>
      <button
        class="shrink-0 text-[12px] font-medium text-accent transition-opacity hover:opacity-75"
        @click="clearFocus"
      >
        Show full timeline
      </button>
    </div>

    <!-- Loading skeleton -->
    <div v-if="state.timelineLoading" class="flex flex-col gap-3">
      <div
        v-for="index in 4"
        :key="index"
        class="h-16 animate-pulse rounded-xl border border-edge bg-panel"
      />
    </div>

    <!-- Empty state -->
    <div
      v-else-if="dayGroups.length === 0"
      class="flex h-full flex-col items-center justify-center gap-3 text-center"
    >
      <span class="grid size-14 place-items-center rounded-2xl border border-edge bg-panel text-dim">
        <UiIcon name="clock" :size="22" />
      </span>
      <p class="text-[13.5px] font-medium text-mut">No recorded changes yet</p>
      <p class="max-w-72 text-[12.5px] leading-relaxed text-dim">
        Undo records every file change while it protects a folder. Start an agent
        with
        <code class="rounded border border-edge bg-well px-1.5 py-0.5 font-mono text-[11px] text-mut">undo run claude</code>
        or just edit files — everything shows up here.
      </p>
    </div>

    <!-- Feed -->
    <div v-else class="flex flex-col gap-3 pb-4">
      <template v-for="group in dayGroups" :key="group.day">
        <div class="mt-1 flex items-center gap-3 first:mt-0">
          <span class="text-[11px] font-semibold uppercase tracking-widest text-dim">
            {{ group.day }}
          </span>
          <span class="h-px flex-1 bg-edge" />
        </div>
        <TimelineItemCard v-for="item in group.items" :key="item.id" :item="item" />
      </template>
    </div>
  </section>
</template>
