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

const changeChip: Record<string, string> = {
  created: 'border-add/30 bg-add/10 text-add',
  deleted: 'border-del/30 bg-del/10 text-del',
  modified: 'border-warn/30 bg-warn/10 text-warn',
  renamed: 'border-accent/30 bg-accent/10 text-accent',
}

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
  <section class="flex min-h-0 flex-col border-l border-edge bg-well/40">
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
        class="flex shrink-0 items-center gap-2.5 border-b border-edge bg-bg/70 px-4 py-2.5 backdrop-blur"
      >
        <span class="min-w-0 flex-1 truncate font-mono text-[12.5px]">
          <span class="text-dim">{{ splitPath(state.diffTarget.file.path).dir }}</span
          ><span class="text-ink">{{ splitPath(state.diffTarget.file.path).name }}</span>
        </span>
        <span
          class="shrink-0 rounded-full border px-2 py-px text-[10.5px] font-medium capitalize"
          :class="changeChip[state.diffTarget.file.change]"
        >
          {{ state.diffTarget.file.change }}
        </span>
        <template v-if="state.diff && !state.diff.binary">
          <span class="shrink-0 font-mono text-[11.5px] text-add">+{{ state.diff.inserted }}</span>
          <span class="shrink-0 font-mono text-[11.5px] text-del">−{{ state.diff.deleted }}</span>
        </template>
        <span
          v-if="pierrePreview"
          class="shrink-0 rounded-full border border-accent/30 bg-accent/10 px-2 py-px text-[10.5px] font-medium text-accent"
        >
          Pierre preview
        </span>
        <button
          v-if="item && item.status !== 'active'"
          class="flex shrink-0 items-center gap-1.5 rounded-lg border border-edge px-2.5 py-1 text-[11.5px] font-medium text-mut transition-colors hover:border-edge-strong hover:text-ink disabled:opacity-40"
          :disabled="state.recoveryBusy"
          title="Preview restoring this file to its state before this item"
          @click="restoreThisFile"
        >
          <!-- <UiIcon name="undo" :size="11" /> -->
          <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="currentColor" class="size-3">
            <path fill-rule="evenodd" d="M7.793 2.232a.75.75 0 0 1-.025 1.06L3.622 7.25h10.003a5.375 5.375 0 0 1 0 10.75H10.75a.75.75 0 0 1 0-1.5h2.875a3.875 3.875 0 0 0 0-7.75H3.622l4.146 3.957a.75.75 0 0 1-1.036 1.085l-5.5-5.25a.75.75 0 0 1 0-1.085l5.5-5.25a.75.75 0 0 1 1.06.025Z" clip-rule="evenodd" />
          </svg>


          Preview file restore
        </button>
      </div>

      <!-- Version labels -->
      <div
        v-if="state.diff"
        class="flex shrink-0 items-center justify-between border-b border-edge px-4 py-1.5 font-mono text-[10.5px] text-dim"
      >
        <span>
          before ·
          {{ state.diff.old_timestamp ? fmtClock(state.diff.old_timestamp) : '—' }}
        </span>
        <span>
          after ·
          {{ state.diff.new_timestamp ? fmtClock(state.diff.new_timestamp) : '—' }}
        </span>
      </div>

      <!-- Diff body -->
      <div class="min-h-0 flex-1 overflow-auto">
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

        <table v-else-if="state.diff" class="w-full border-collapse font-mono text-[12px]">
          <tbody>
            <template v-for="(hunk, hunkIndex) in state.diff.hunks" :key="hunkIndex">
              <tr class="bg-panel">
                <td
                  colspan="3"
                  class="select-none px-4 py-1.5 text-[10.5px] tracking-wide text-dim"
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
                  class="w-11 select-none border-r border-edge/60 px-2 text-right align-top text-[10.5px] leading-[1.7] text-dim"
                >
                  {{ line.old_line ?? '' }}
                </td>
                <td
                  class="w-11 select-none border-r border-edge/60 px-2 text-right align-top text-[10.5px] leading-[1.7] text-dim"
                >
                  {{ line.new_line ?? '' }}
                </td>
                <td class="whitespace-pre-wrap break-all px-3 leading-[1.7]">
                  <span
                    class="mr-2 inline-block w-2 select-none"
                    :class="{
                      'text-add': line.kind === 'add',
                      'text-del': line.kind === 'del',
                      'text-transparent': line.kind === 'ctx',
                    }"
                    >{{ line.kind === 'add' ? '+' : line.kind === 'del' ? '−' : ' ' }}</span
                  ><span
                    :class="{
                      'text-add': line.kind === 'add',
                      'text-del': line.kind === 'del',
                      'text-mut': line.kind === 'ctx',
                    }"
                    >{{ line.text }}</span
                  >
                </td>
              </tr>
            </template>
          </tbody>
        </table>
      </div>
    </template>
  </section>
</template>
