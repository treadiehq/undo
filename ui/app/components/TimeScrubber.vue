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
  for (const item of state.timeline?.items ?? []) {
    for (const file of item.files) {
      if (file.last_timestamp <= selected.value) continue
      paths.add(file.path)
    }
  }
  return { files: paths.size }
})

function bucketHeight(count: number): string {
  if (count === 0) return '3px'
  return `${3 + Math.round((count / maxBucketCount.value) * 19)}px`
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
  <section class="shrink-0 border-b border-edge bg-well px-5 pt-4 pb-3 sm:px-6" aria-label="Restore point">
    <div class="flex flex-wrap items-center justify-between gap-x-5 gap-y-3">
      <details class="relative z-30">
        <summary class="list-none rounded-sm [&::-webkit-details-marker]:hidden" aria-label="Set exact restore time">
          <span class="block text-[12px] text-mut">Restore to</span>
          <span class="mt-0.5 flex items-center gap-2 text-[18px] font-medium tracking-tight text-ink">
            {{ fmtDay(selected) }}, {{ fmtClock(selected) }}
            <UiIcon name="chevron" :size="12" class="text-mut" />
          </span>
        </summary>
        <label class="absolute left-0 top-full mt-2 block rounded-lg border border-edge-strong bg-panel p-3 shadow-xl shadow-black/30">
          <span class="mb-2 block text-[12px] text-mut">Exact restore time</span>
          <input
            v-model="datetimeValue"
            type="datetime-local"
            :min="toLocalDatetime(historyStart)"
            :max="toLocalDatetime(serverNow)"
            :disabled="!hasHistory"
            class="max-w-[75vw] rounded border border-edge-strong bg-well px-2 py-2 text-[13px] text-ink disabled:opacity-40"
          />
        </label>
      </details>
      <div class="flex items-center gap-5">
        <p class="text-right text-[12px] text-mut">
          <span class="text-ink">{{ impact.files }} file{{ impact.files === 1 ? '' : 's' }}</span> after this point
          <span class="mt-0.5 block text-[11px]">Review before restoring</span>
        </p>
        <button class="ui-button ui-button-primary" :disabled="!hasHistory || state.recoveryBusy" aria-label="Preview time-based restore" @click="previewSelected">
          Preview
          <UiIcon name="chevron" :size="13" class="-rotate-90" />
        </button>
      </div>
    </div>

    <div class="relative mt-3 h-8">
      <div class="absolute inset-x-1 bottom-2 flex h-6 items-end gap-1 overflow-hidden" aria-hidden="true">
        <span
          v-for="(bucket, index) in activityBuckets"
          :key="index"
          class="min-w-0 flex-1 rounded-t-[1px]"
          :class="bucket.timestamp > selected ? 'bg-accent/45' : 'bg-edge-strong/60'"
          :style="{ height: bucketHeight(bucket.count) }"
          :title="`${bucket.count} recorded change${bucket.count === 1 ? '' : 's'}`"
        />
      </div>
      <input
        v-model.number="selected"
        type="range"
        :min="historyStart"
        :max="serverNow"
        :step="60"
        :disabled="!hasHistory"
        :aria-valuetext="`${fmtDay(selected)} at ${fmtClock(selected)}`"
        class="restore-range absolute inset-x-0 bottom-0 z-10 h-4 w-full disabled:cursor-not-allowed disabled:opacity-40"
        aria-label="Choose restore time"
      />
    </div>
    <div class="mt-0.5 flex items-center justify-between gap-3 text-[11px] tabular-nums text-dim">
      <span>{{ fmtDay(historyStart) }} {{ fmtClock(historyStart) }}</span>
      <span class="hidden sm:inline">Changes after this point</span>
      <span>Now · {{ fmtClock(serverNow) }}</span>
    </div>
  </section>
</template>

<style scoped>
.restore-range { appearance: none; background: transparent; }
.restore-range::-webkit-slider-runnable-track { height: 2px; background: var(--color-edge-strong); }
.restore-range::-webkit-slider-thumb {
  width: 12px; height: 12px; margin-top: -5px; appearance: none;
  border: 2px solid var(--color-accent); border-radius: 50%;
  background: var(--color-well); box-shadow: 0 0 0 3px var(--color-well); cursor: ew-resize;
}
.restore-range::-moz-range-track { height: 2px; background: var(--color-edge-strong); }
.restore-range::-moz-range-thumb {
  width: 9px; height: 9px; border: 2px solid var(--color-accent); border-radius: 50%;
  background: var(--color-well); box-shadow: 0 0 0 3px var(--color-well); cursor: ew-resize;
}
</style>
