<script setup lang="ts">
import { computed, watch } from 'vue'
import { fmtClock, fmtDay } from '~/utils/format'

const { state, previewTimestamp, setRestoreTimestamp } = useUndo()
const BUCKET_COUNT = 32

const serverNow = computed(() => state.timeline?.now ?? 0)
const historyStart = computed(() => {
  const projectStart = state.timeline?.project.first_event_at
  if (projectStart != null) return projectStart
  const items = state.timeline?.items ?? []
  if (items.length === 0) return serverNow.value
  return Math.min(...items.map((item) => item.started_at))
})
const rewindTimestamp = computed(() => serverNow.value - 10 * 60)

watch(
  () => state.projectId,
  () => {
    setRestoreTimestamp(null)
  },
)

watch(
  [historyStart, serverNow],
  ([start, end]) => {
    if (state.restoreTimestamp === null) {
      setRestoreTimestamp(Math.max(start, end - 10 * 60))
      return
    }
    setRestoreTimestamp(Math.min(end, Math.max(start, state.restoreTimestamp)))
  },
  { immediate: true },
)

const selected = computed({
  get: () => state.restoreTimestamp ?? rewindTimestamp.value,
  set: (value: number) => {
    setRestoreTimestamp(
      Math.min(serverNow.value, Math.max(historyStart.value, value)),
    )
  },
})

const datetimeValue = computed({
  get: () => toLocalDatetime(selected.value),
  set: (value: string) => {
    const milliseconds = new Date(value).getTime()
    if (!Number.isNaN(milliseconds)) selected.value = Math.floor(milliseconds / 1000)
  },
})

const hasHistory = computed(() => historyStart.value < serverNow.value)
const selectedPercent = computed(() => {
  const duration = serverNow.value - historyStart.value
  if (duration <= 0) return 0
  return ((selected.value - historyStart.value) / duration) * 100
})
const activityBuckets = computed(() => {
  const start = historyStart.value
  const duration = Math.max(1, serverNow.value - start)
  const buckets = Array.from({ length: BUCKET_COUNT }, (_, index) => ({
    count: 0,
    timestamp: start + ((index + 0.5) / BUCKET_COUNT) * duration,
  }))
  for (const item of state.timeline?.items ?? []) {
    const timestamp = Math.min(
      serverNow.value,
      Math.max(start, item.ended_at ?? item.started_at),
    )
    const index = Math.min(
      BUCKET_COUNT - 1,
      Math.floor(((timestamp - start) / duration) * BUCKET_COUNT),
    )
    buckets[index]!.count += Math.max(1, item.event_count)
  }
  return buckets
})
const maxBucketCount = computed(() =>
  Math.max(1, ...activityBuckets.value.map((bucket) => bucket.count)),
)
const impact = computed(() => {
  const paths = new Set<string>()
  let changes = 0
  for (const item of state.timeline?.items ?? []) {
    for (const file of item.files) {
      if (file.last_timestamp <= selected.value) continue
      paths.add(file.path)
      changes += file.event_count
    }
  }
  return { files: paths.size, changes }
})

function bucketHeight(count: number): string {
  if (count === 0) return '3px'
  return `${6 + Math.round((count / maxBucketCount.value) * 28)}px`
}

function toLocalDatetime(timestamp: number): string {
  const date = new Date(timestamp * 1000)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

function previewAt(timestamp: number, request: string) {
  void previewTimestamp({ timestamp, description: request })
}

function previewSelected() {
  previewAt(
    selected.value,
    `Restore the project to ${fmtDay(selected.value)} at ${fmtClock(selected.value)}`,
  )
}
</script>

<template>
  <section class="shrink-0 border-b border-edge bg-panel/60 px-5 py-3 backdrop-blur">
    <div class="mb-2 flex items-end gap-4">
      <div class="min-w-0 flex-1">
        <p class="text-[10.5px] text-dim">Restore point</p>
        <p class="text-[13px] font-semibold text-ink">
          {{ fmtDay(selected) }} at {{ fmtClock(selected) }}
        </p>
      </div>
      <p class="shrink-0 text-right text-[11px] text-mut">
        <span class="font-semibold text-ink">{{ impact.changes }}</span> recorded
        change{{ impact.changes === 1 ? '' : 's' }} across
        <span class="font-semibold text-ink">{{ impact.files }}</span>
        file{{ impact.files === 1 ? '' : 's' }}
      </p>
      <label class="shrink-0">
        <span class="sr-only">Exact restore time</span>
        <input
          v-model="datetimeValue"
          type="datetime-local"
          :min="toLocalDatetime(historyStart)"
          :max="toLocalDatetime(serverNow)"
          :disabled="!hasHistory"
          class="rounded-lg border border-edge bg-well px-3 py-1.5 font-mono text-[11.5px] text-ink outline-none transition-colors focus:border-edge-strong disabled:opacity-40"
        />
      </label>
      <button
        class="flex shrink-0 items-center gap-2 rounded-lg bg-ink px-4 py-2 text-[12.5px] font-semibold text-bg transition-opacity hover:opacity-85 disabled:cursor-not-allowed disabled:opacity-40"
        :disabled="!hasHistory || state.recoveryBusy"
        @click="previewSelected"
      >
        <!-- <UiIcon name="undo" :size="13" /> -->
        Preview
      </button>
    </div>

    <div class="relative h-15.5">
      <div class="absolute inset-x-0 bottom-4 top-2 flex items-end gap-1 overflow-hidden">
        <span
          v-for="(bucket, index) in activityBuckets"
          :key="index"
          class="min-w-0 flex-1 rounded-t-sm transition-colors"
          :class="bucket.timestamp > selected ? 'bg-accent/75' : 'bg-edge-strong'"
          :style="{ height: bucketHeight(bucket.count) }"
          :title="`${bucket.count} recorded change${bucket.count === 1 ? '' : 's'}`"
        />
      </div>

      <span
        class="pointer-events-none absolute bottom-4 right-0 top-2 border-l border-accent/60 bg-accent/5"
        :style="{ left: `${selectedPercent}%` }"
      />

      <span
        class="pointer-events-none absolute top-0 z-20 -translate-x-1/2 whitespace-nowrap rounded-md border border-edge-strong bg-panel px-2 py-1 text-[10.5px] font-semibold text-ink"
        :style="{
          left: `clamp(3rem, ${selectedPercent}%, calc(100% - 3rem))`,
        }"
      >
        {{ impact.files }} file{{ impact.files === 1 ? '' : 's' }}
      </span>

      <input
        v-model.number="selected"
        type="range"
        :min="historyStart"
        :max="serverNow"
        :step="60"
        :disabled="!hasHistory"
        class="restore-range absolute inset-x-0 bottom-2.25 z-10 h-4 w-full cursor-pointer disabled:cursor-not-allowed disabled:opacity-40"
        aria-label="Choose restore time"
      />

      <div class="absolute inset-x-0 bottom-0 flex justify-between text-[10px] text-dim">
        <span>{{ fmtDay(historyStart) }} {{ fmtClock(historyStart) }}</span>
        <span>Changes to undo</span>
        <span>Now · {{ fmtClock(serverNow) }}</span>
      </div>
    </div>

    <p class="mt-1 text-right text-[10.5px] text-dim">
      Dragging previews impact in the timeline below. Files change only after
      reviewing and applying the plan.
    </p>
  </section>
</template>

<style scoped>
.restore-range {
  appearance: none;
  background: transparent;
}

.restore-range::-webkit-slider-runnable-track {
  height: 2px;
  background: var(--color-edge-strong);
}

.restore-range::-webkit-slider-thumb {
  width: 14px;
  height: 14px;
  margin-top: -6px;
  appearance: none;
  border: 2px solid var(--color-panel);
  border-radius: 999px;
  background: var(--color-ink);
}

.restore-range::-moz-range-track {
  height: 2px;
  background: var(--color-edge-strong);
}

.restore-range::-moz-range-thumb {
  width: 14px;
  height: 14px;
  border: 2px solid var(--color-panel);
  border-radius: 999px;
  background: var(--color-ink);
}
</style>
