<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import type { DiffPayload } from '~/types'

const props = defineProps<{ diff: DiffPayload }>()

const container = ref<HTMLElement | null>(null)
const error = ref<string | null>(null)

interface PierreInstance {
  cleanUp(recycle?: boolean): void
}

let instance: PierreInstance | null = null
let renderGeneration = 0

function asPatch(diff: DiffPayload): string {
  const oldPath = diff.change === 'created' ? '/dev/null' : `a/${diff.path}`
  const newPath = diff.change === 'deleted' ? '/dev/null' : `b/${diff.path}`
  const lines = [`--- ${oldPath}`, `+++ ${newPath}`]

  for (const hunk of diff.hunks) {
    lines.push(hunk.header)
    for (const line of hunk.lines) {
      const prefix = line.kind === 'add' ? '+' : line.kind === 'del' ? '-' : ' '
      lines.push(`${prefix}${line.text}`)
    }
  }

  return `${lines.join('\n')}\n`
}

async function renderDiff() {
  const root = container.value
  if (!root) return

  const generation = ++renderGeneration
  instance?.cleanUp()
  instance = null
  root.replaceChildren()
  error.value = null

  try {
    // The experiment is code-split: Pierre is only evaluated when the
    // `?diff=pierre` renderer mounts.
    const { FileDiff, processFile } = await import('@pierre/diffs')
    if (generation !== renderGeneration) return

    const fileDiff = processFile(asPatch(props.diff), {
      cacheKey: [
        props.diff.path,
        props.diff.old_timestamp,
        props.diff.new_timestamp,
      ].join(':'),
      isGitDiff: false,
      throwOnError: true,
    })
    if (!fileDiff) throw new Error('Pierre could not parse this diff')

    const next = new FileDiff({
      theme: 'pierre-dark',
      themeType: 'dark',
      diffStyle: 'unified',
      diffIndicators: 'bars',
      lineDiffType: 'word-alt',
      overflow: 'wrap',
      disableFileHeader: true,
      hunkSeparators: 'line-info-basic',
    })
    next.render({ fileDiff, containerWrapper: root })
    instance = next
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

onMounted(() => void renderDiff())
watch(() => props.diff, () => void renderDiff(), { deep: true })

onBeforeUnmount(() => {
  renderGeneration += 1
  instance?.cleanUp()
  instance = null
})
</script>

<template>
  <div class="min-h-full min-w-full">
    <div
      v-if="error"
      class="m-4 rounded-lg border border-del/30 bg-del/10 px-3 py-2 text-[12px] text-del"
    >
      Pierre preview failed: {{ error }}
    </div>
    <div ref="container" />
  </div>
</template>
