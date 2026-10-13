<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { fmtClock, splitPath } from '~/utils/format'

const { state, previewUndo } = useUndo()
const pierrePreview = ref(false)

onMounted(() => {
  pierrePreview.value =
    new URLSearchParams(window.location.search).get('diff') === 'pierre'
})

const item = computed(() =>
  state.timeline?.items.find((entry) => entry.id === state.diffTarget?.itemId),
)

const selectedForRestore = computed(() => {
  const target = state.diffTarget
  return !!target && !!state.selections.get(target.itemId)?.has(target.file.path)
})

function restoreThisFile() {
  const target = state.diffTarget
  const owner = item.value
  if (!target || !owner) return
  void previewUndo({
    item: owner,
    paths: [target.file.path],
    description: `Restore ${target.file.path} to before ${owner.label}`,
  })
}
</script>

<template>
  <section class="diff-panel flex min-h-0 min-w-0 flex-col border-l border-edge bg-bg" aria-label="File changes">
    <!-- Empty state -->
    <div
      v-if="!state.diffTarget"
      class="flex flex-1 flex-col items-center justify-center gap-3 p-8 text-center"
    >
      <span
        class="grid size-14 place-items-center rounded-2xl border border-edge bg-panel text-dim"
      >
        <UiIcon name="file" :size="22" />
      </span>
      <p class="text-[13px] font-medium text-mut">Select a file to review its changes</p>
      <p class="max-w-60 text-[12px] leading-relaxed text-dim">
        Every version is saved locally. Pick any file from the timeline to see
        exactly what changed.
      </p>
    </div>

    <template v-else>
      <!-- Diff header -->
      <div
        class="flex shrink-0 flex-wrap items-start justify-between gap-x-4 gap-y-2 border-b border-edge px-5 py-4"
      >
        <div class="min-w-0 flex-1">
          <p class="break-all text-[11px] text-mut">{{ splitPath(state.diffTarget.file.path).dir || 'Project root' }}</p>
          <h2 class="mt-1 break-all font-mono text-[13px] font-medium text-ink">{{ splitPath(state.diffTarget.file.path).name }}</h2>
        </div>
        <div class="flex shrink-0 items-center gap-2.5 text-[11px]">
          <span v-if="state.diffTarget.file.change !== 'modified'" class="capitalize text-mut">{{ state.diffTarget.file.change }}</span>
          <template v-if="state.diff && !state.diff.binary">
            <span class="font-mono tabular-nums text-add">+{{ state.diff.inserted }}</span>
            <span class="font-mono tabular-nums text-del">−{{ state.diff.deleted }}</span>
          </template>
        </div>
        <span
          v-if="pierrePreview"
          class="shrink-0 rounded-full border border-accent/30 bg-accent/10 px-2 py-px text-[10.5px] font-medium text-accent"
        >
          Pierre preview
        </span>
      </div>

      <!-- Version labels -->
      <div
        v-if="state.diff"
        class="flex shrink-0 flex-wrap items-center justify-between gap-2 border-b border-edge px-5 py-2.5 text-[11px] tabular-nums text-mut"
      >
        <span>
          Before ·
          {{ state.diff.old_timestamp ? fmtClock(state.diff.old_timestamp) : '—' }}
        </span>
        <span>
          After ·
          {{ state.diff.new_timestamp ? fmtClock(state.diff.new_timestamp) : '—' }}
        </span>
      </div>

      <!-- Diff body -->
      <div class="diff-body min-h-0 flex-1 overflow-auto">
        <div v-if="state.diffLoading" class="flex h-full items-center justify-center">
          <span class="text-[12px] text-dim">Computing diff…</span>
        </div>

        <div
          v-else-if="state.diffTarget.file.binary || state.diff?.binary"
          class="flex h-full flex-col items-center justify-center gap-2 text-center"
        >
          <p class="text-[13px] text-mut">Binary file</p>
          <p class="text-[12px] text-dim">
            Content is stored byte-for-byte and restores exactly, but there is no
            text diff to show.
          </p>
        </div>

        <div
          v-else-if="state.diff && state.diff.hunks.length === 0"
          class="flex h-full items-center justify-center"
        >
          <p class="text-[12px] text-dim">No line differences in this range.</p>
        </div>

        <PierreDiff
          v-else-if="state.diff && pierrePreview"
          :diff="state.diff"
        />

        <table v-else-if="state.diff" class="w-full table-fixed border-collapse font-mono text-[12.5px]" aria-label="Recorded line changes">
          <colgroup><col class="w-9" /><col class="w-9" /><col /></colgroup>
          <tbody>
            <template v-for="(hunk, hunkIndex) in state.diff.hunks" :key="hunkIndex">
              <tr class="border-y border-edge bg-well">
                <td
                  colspan="3"
                  class="select-none px-5 py-2 text-[11px] text-dim"
                >
                  {{ hunk.header }}
                </td>
              </tr>
              <tr
                v-for="(line, lineIndex) in hunk.lines"
                :key="`${hunkIndex}-${lineIndex}`"
                :class="{
                  'bg-add-soft': line.kind === 'add',
                  'bg-del-soft': line.kind === 'del',
                }"
              >
                <td
                  class="select-none pr-2 text-right align-top text-[11px] leading-[2.1] text-dim"
                >
                  {{ line.old_line ?? '' }}
                </td>
                <td
                  class="select-none pr-2 text-right align-top text-[11px] leading-[2.1] text-dim"
                >
                  {{ line.new_line ?? '' }}
                </td>
                <td class="whitespace-pre-wrap break-words py-px pr-5 leading-[1.85] [overflow-wrap:anywhere]">
                  <span
                    class="mr-2 inline-block w-2 select-none"
                    :class="{
                      'text-add': line.kind === 'add',
                      'text-del': line.kind === 'del',
                      'text-transparent': line.kind === 'ctx',
                    }"
                    >{{ line.kind === 'add' ? '+' : line.kind === 'del' ? '−' : ' ' }}</span
                  ><span class="text-ink">{{ line.text }}</span
                  >
                </td>
              </tr>
            </template>
          </tbody>
        </table>
      </div>
      <div class="flex shrink-0 flex-wrap items-center justify-between gap-3 border-t border-edge px-5 py-3">
        <span class="flex min-w-0 items-center gap-2 text-[11px] text-mut">
          <UiIcon v-if="selectedForRestore" name="check" :size="13" class="shrink-0" />
          {{ state.diffTarget.file.warning ?? (selectedForRestore ? 'Selected for run restore' : 'Recorded changes for this activity') }}
        </span>
        <button
          v-if="item && item.status !== 'active' && state.diffTarget.file.recoverable"
          class="ui-button"
          :disabled="state.recoveryBusy"
          title="Restore this file to before this activity"
          @click="restoreThisFile"
        >Preview file restore</button>
      </div>
    </template>
  </section>
</template>
