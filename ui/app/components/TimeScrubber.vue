<script setup lang="ts">
import { computed, ref } from 'vue'
import type { TimelineItem } from '~/types'
import { fmtClock, fmtDay } from '~/utils/format'

const { state, toggleExpanded } = useUndo()

const WIDTH = 1000
const HEIGHT = 56
const BUCKETS = 140

const items = computed(() => state.timeline?.items ?? [])

const domain = computed(() => {
  const now = state.timeline?.now ?? Math.floor(Date.now() / 1000)
  if (items.value.length === 0) return { start: now - 3600, end: now }
  const start = Math.min(...items.value.map((item) => item.started_at))
  // Leave breathing room on the right so "now" is not glued to the edge.
  const span = Math.max(now - start, 600)
  return { start, end: now + span * 0.03 }
})

function x(ts: number): number {
  const { start, end } = domain.value
  return ((ts - start) / (end - start)) * WIDTH
}

// Change-density histogram: every item spreads its events across its
// duration, giving the seismograph strip under the markers.
const bars = computed(() => {
  const counts = new Array<number>(BUCKETS).fill(0)
  const { start, end } = domain.value
  const bucketSpan = (end - start) / BUCKETS
  for (const item of items.value) {
    const from = item.started_at
    const to = Math.max(item.ended_at ?? item.started_at, from + 1)
    const firstBucket = Math.floor((from - start) / bucketSpan)
    const lastBucket = Math.min(Math.floor((to - start) / bucketSpan), BUCKETS - 1)
    const spread = lastBucket - firstBucket + 1
    for (let bucket = firstBucket; bucket <= lastBucket; bucket += 1) {
      if (bucket >= 0 && bucket < BUCKETS) counts[bucket]! += item.event_count / spread
    }
  }
  const max = Math.max(...counts, 1)
  return counts.map((count, index) => ({
    x: (index / BUCKETS) * WIDTH,
    height: count === 0 ? 0 : 3 + (count / max) * (HEIGHT - 26),
  }))
})

const hovered = ref<TimelineItem | null>(null)

// Anchor the tooltip so it never clips at the viewport edges: markers near
// the left edge grow it rightward, near the right edge leftward, and
// everything else centers. It always sits fully above the strip.
const tooltipStyle = computed(() => {
  if (!hovered.value) return {}
  const pct = (x(hovered.value.started_at) / WIDTH) * 100
  if (pct < 12) {
    return { left: `${Math.max(pct, 0)}%`, transform: 'translateY(-100%)' }
  }
  if (pct > 88) {
    return {
      right: `${Math.max(100 - pct, 0)}%`,
      transform: 'translateY(-100%)',
    }
  }
  return { left: `${pct}%`, transform: 'translateX(-50%) translateY(-100%)' }
})

// The whole strip is the hit target, not just the 3.5px dots: pointing
// anywhere inside an item's activity span (its histogram bars) counts, and
// so does ~2.5% of the width around it.
const HIT_TOLERANCE = 25

function nearestItem(event: MouseEvent): TimelineItem | null {
  const svg = event.currentTarget as SVGElement
  const rect = svg.getBoundingClientRect()
  const vx = ((event.clientX - rect.left) / rect.width) * WIDTH
  let best: TimelineItem | null = null
  let bestDistance = HIT_TOLERANCE
  for (const item of items.value) {
    const left = x(item.started_at)
    const right = x(Math.max(item.ended_at ?? item.started_at, item.started_at))
    const distance = vx < left ? left - vx : vx > right ? vx - right : 0
    // `<=` so that among overlapping candidates the later (more specific,
    // newest-last in this loop) item wins.
    if (distance <= bestDistance) {
      best = item
      bestDistance = distance
    }
  }
  return best
}

function onMove(event: MouseEvent) {
  hovered.value = nearestItem(event)
}

function onClick(event: MouseEvent) {
  const item = nearestItem(event)
  if (item) jumpTo(item)
}

function jumpTo(item: TimelineItem) {
  if (!state.expanded.has(item.id)) toggleExpanded(item.id)
  const element = document.getElementById(`item-${item.id}`)
  element?.scrollIntoView({ behavior: 'smooth', block: 'center' })
  // Flash the card so the jump is visible even when it was already on
  // screen — with few items, the scroll alone can be imperceptible.
  element?.animate(
    [
      {
        boxShadow:
          '0 0 0 2px color-mix(in srgb, var(--color-accent) 70%, transparent)',
      },
      {
        boxShadow:
          '0 0 0 2px color-mix(in srgb, var(--color-accent) 0%, transparent)',
      },
    ],
    { duration: 1100, easing: 'ease-out' },
  )
}
</script>

<template>
  <footer class="shrink-0 border-t border-edge bg-panel/60 px-5 pb-2.5 pt-2 backdrop-blur">
    <div class="relative">
      <svg
        :viewBox="`0 0 ${WIDTH} ${HEIGHT}`"
        preserveAspectRatio="none"
        class="h-14 w-full"
        :class="hovered ? 'cursor-pointer' : ''"
        @mousemove="onMove"
        @mouseleave="hovered = null"
        @click="onClick"
      >
        <!-- density strip -->
        <rect
          v-for="(bar, index) in bars"
          :key="index"
          :x="bar.x"
          :y="HEIGHT - 12 - bar.height"
          :width="WIDTH / BUCKETS - 1.2"
          :height="bar.height"
          rx="1"
          class="fill-edge"
        />
        <!-- axis -->
        <line
          x1="0"
          :y1="HEIGHT - 10"
          :x2="WIDTH"
          :y2="HEIGHT - 10"
          class="stroke-edge"
          stroke-width="1"
        />
        <!-- item markers (visual only; the svg handles hit detection) -->
        <circle
          v-for="item in items"
          :key="item.id"
          :cx="x(item.started_at)"
          :cy="HEIGHT - 10"
          :r="hovered?.id === item.id ? 5 : 3.5"
          :class="item.kind === 'run' ? 'fill-accent' : 'fill-mut'"
          class="pointer-events-none transition-all"
        />
        <!-- now marker -->
        <circle :cx="x(domain.end)" :cy="HEIGHT - 10" r="3" class="fill-live" />
      </svg>

      <!-- hover tooltip -->
      <div
        v-if="hovered"
        class="pointer-events-none absolute -top-1 z-30 whitespace-nowrap rounded-lg border border-edge bg-bg px-2.5 py-1.5 text-[11px] shadow-xl shadow-black/50"
        :style="tooltipStyle"
      >
        <span class="font-medium text-ink">{{ hovered.label }}</span>
        <span class="text-dim">
          · {{ fmtClock(hovered.started_at) }} · {{ hovered.file_count }} files</span
        >
      </div>
    </div>

    <div class="flex justify-between font-mono text-[10.5px] text-dim">
      <span>{{ fmtDay(domain.start) }} {{ fmtClock(domain.start) }}</span>
      <span class="text-live">now</span>
    </div>
  </footer>
</template>
