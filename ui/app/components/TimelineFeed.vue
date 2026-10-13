<script setup lang="ts">
import { computed } from 'vue'
import type { TimelineItem } from '~/types'
import { fmtDay } from '~/utils/format'

const { state, focusedItem, clearFocus, visibleTimelineItems } = useUndo()

// Items grouped under day separators (newest first, matching payload order).
// In focus mode (`undo ui r_421`) the feed shows only the reviewed Run; the
// full timeline is one click away.
const dayGroups = computed(() => {
  const source = visibleTimelineItems.value
  const groups: Array<{ day: string; items: TimelineItem[] }> = []
  for (const item of source) {
    const day = fmtDay(item.started_at)
    const last = groups[groups.length - 1]
    if (last && last.day === day) last.items.push(item)
    else groups.push({ day, items: [item] })
  }
  return groups
})
const feedRows = computed(() => dayGroups.value.flatMap((group) => [
  { key: `day-${group.day}`, day: group.day, item: null as TimelineItem | null },
  ...group.items.map((item) => ({ key: item.id, day: group.day, item })),
]))
</script>

<template>
  <section class="timeline-feed min-h-0 min-w-0 overflow-y-auto bg-panel" aria-label="Recorded activity" :aria-busy="state.restorePreviewLoading">
    <!-- Panic alert: a recent un-attributed group deleted multiple files -->
    <PanicBanner v-if="!state.timelineLoading" class="mx-4 mt-4" />

    <!-- Focus mode banner -->
    <div
      v-if="focusedItem"
      class="mx-4 mt-4 mb-3 flex items-center gap-2.5 rounded-xl border border-accent/25 bg-accent/5 px-4 py-2.5"
    >
      <UiIcon name="bot" :size="13" class="shrink-0 text-accent" />
      <span class="min-w-0 flex-1 truncate text-[12.5px] text-mut">
        Reviewing
        <span class="font-semibold text-ink">{{ focusedItem.label }}</span>
      </span>
      <button
        class="shrink-0 text-[12px] font-medium text-accent transition-opacity hover:opacity-75"
        @click="clearFocus"
      >
        Show full timeline
      </button>
    </div>

    <!-- Loading skeleton -->
    <div v-if="state.timelineLoading || (state.restorePreviewLoading && !state.restorePreview)" class="flex flex-col gap-3 p-5">
      <div
        v-for="index in 4"
        :key="index"
        class="h-16 animate-pulse rounded-xl border border-edge bg-panel"
      />
    </div>

    <div v-if="state.restorePreviewError" class="px-5 py-4 text-[12px] text-warn" role="status">
      {{ state.restorePreviewError }}
    </div>

    <!-- Feed -->
    <AnimatedList class="pb-4">
      <div v-for="row in feedRows" :key="row.key">
        <div v-if="!row.item" class="px-5 pt-5 pb-1">
          <span class="text-[11px] font-medium uppercase tracking-widest text-dim">{{ row.day }}</span>
        </div>
        <TimelineItemCard
          v-else
          :item="row.item"
          :time-preview="state.restoreTimestamp !== null"
        />
      </div>
      <div
        v-if="dayGroups.length === 0 && !state.timelineLoading && !state.restorePreviewLoading && !state.restorePreviewError"
        key="empty"
      >
        <div class="flex min-h-64 flex-col items-center justify-center gap-2 px-6 text-center">
          <p class="text-[13px] font-medium text-mut">{{ state.restorePreview ? 'No affected files here' : 'No recorded changes yet' }}</p>
          <p class="max-w-60 text-[12px] leading-relaxed text-dim">{{ state.restorePreview ? 'Choose another restore point to see its changes.' : 'Recorded file changes will appear here.' }}</p>
        </div>
      </div>
    </AnimatedList>
  </section>
</template>
