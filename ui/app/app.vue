<script setup lang="ts">
import { onMounted } from 'vue'

const { state, boot } = useUndo()

onMounted(() => {
  void boot()
})
</script>

<template>
  <div class="flex h-full flex-col overflow-hidden">
    <!-- Fatal error (bad/missing token, server unreachable) -->
    <div
      v-if="state.fatal"
      class="flex flex-1 flex-col items-center justify-center gap-4 p-8 text-center"
    >
      <img src="/logo.png" alt="Undo" class="size-14" />
      <h1 class="text-[15px] font-semibold text-ink">Cannot open the Undo UI</h1>
      <p class="max-w-96 text-[13px] leading-relaxed text-mut">{{ state.fatal }}</p>
      <code class="rounded-lg border border-edge bg-panel px-3 py-1.5 font-mono text-[12px] text-mut">
        undo ui
      </code>
    </div>

    <!-- Booting -->
    <div v-else-if="!state.ready" class="flex flex-1 items-center justify-center">
      <span class="text-[13px] text-dim">Connecting…</span>
    </div>

    <!-- No watched projects yet -->
    <div
      v-else-if="state.projects.length === 0"
      class="flex flex-1 flex-col items-center justify-center gap-4 p-8 text-center"
    >
      <span class="grid size-14 place-items-center rounded-2xl border border-edge bg-panel text-dim">
        <UiIcon name="folder" :size="22" />
      </span>
      <h1 class="text-[15px] font-semibold text-ink">No protected folders yet</h1>
      <p class="max-w-96 text-[13px] leading-relaxed text-mut">
        Undo has not recorded any project on this machine. Protect one first,
        then reload this page.
      </p>
      <code class="rounded-lg border border-edge bg-panel px-3 py-1.5 font-mono text-[12px] text-mut">
        cd my-project && undo start
      </code>
    </div>

    <!-- Main app -->
    <template v-else>
      <AppHeader />
      <main class="grid min-h-0 flex-1 grid-cols-[minmax(24rem,5fr)_minmax(20rem,6fr)]">
        <TimelineFeed />
        <DiffPanel />
      </main>
      <TimeScrubber />
    </template>

    <RecoveryModal />
    <ToastStack />
  </div>
</template>
