<script setup lang="ts">
import { ref } from 'vue'

const { state, currentProject, selectProject } = useUndo()
const menuOpen = ref(false)

function choose(id: number) {
  menuOpen.value = false
  void selectProject(id)
}
</script>

<template>
  <header
    class="flex h-14 shrink-0 items-center gap-4 border-b border-edge bg-bg/90 px-5 backdrop-blur"
  >
    <div class="flex items-center gap-1.5">
      <!-- <span
        class="grid size-7 place-items-center rounded-lg border border-edge bg-panel text-ink"
      >
        <UiIcon name="undo" :size="15" />
      </span> -->
      <img src="/logo.png" alt="Undo" class="size-6" />
      <span class="text-[15px] font-semibold tracking-tight">Undo</span>
    </div>

    <!-- Project switcher -->
    <div v-if="currentProject" class="relative">
      <button
        class="flex items-center gap-2 rounded-lg border border-edge bg-panel px-3 py-1.5 text-[13px] text-mut transition-colors hover:border-edge-strong hover:text-ink"
        @click="menuOpen = !menuOpen"
      >
        <UiIcon name="folder" :size="13" class="text-dim" />
        <span class="font-medium text-ink">{{ currentProject.name }}</span>
        <UiIcon
          name="chevron"
          :size="12"
          class="text-dim transition-transform"
          :class="menuOpen ? 'rotate-180' : ''"
        />
      </button>
      <Transition name="fade">
        <div
          v-if="menuOpen"
          class="absolute left-0 top-full z-40 mt-1.5 w-72 overflow-hidden rounded-xl border border-edge bg-panel shadow-2xl shadow-black/60"
        >
          <button
            v-for="project in state.projects"
            :key="project.id"
            class="flex w-full items-center gap-2.5 px-3.5 py-2.5 text-left text-[13px] transition-colors hover:bg-well"
            @click="choose(project.id)"
          >
            <span
              class="size-1.5 shrink-0 rounded-full"
              :class="project.recording ? 'bg-live' : 'bg-dim'"
            />
            <span class="min-w-0 flex-1">
              <span class="block truncate font-medium text-ink">{{ project.name }}</span>
              <span class="block truncate font-mono text-[11px] text-dim">{{
                project.root_path
              }}</span>
            </span>
            <UiIcon
              v-if="project.id === state.projectId"
              name="check"
              :size="13"
              class="shrink-0 text-mut"
            />
          </button>
        </div>
      </Transition>
    </div>

    <div class="flex-1" />

    <!-- Active run indicator -->
    <span
      v-if="state.activeRunId"
      class="flex items-center gap-2 rounded-full border border-accent/30 bg-accent/10 px-3 py-1 text-[12px] font-medium text-accent"
    >
      <UiIcon name="sparkle" :size="12" />
      Run {{ state.activeRunId }} in progress
    </span>

    <!-- Recording status -->
    <span
      class="flex items-center gap-2 rounded-full border px-3 py-1 text-[12px] font-medium"
      :class="
        state.recording
          ? 'border-live/25 bg-live/10 text-live'
          : 'border-warn/25 bg-warn/10 text-warn'
      "
    >
      <span
        class="size-1.5 rounded-full"
        :class="state.recording ? 'bg-live live-dot' : 'bg-warn'"
      />
      {{ state.recording ? 'Protected' : 'Not recording' }}
    </span>

    <!-- <span class="font-mono text-[11px] text-dim">v{{ state.version }}</span> -->
  </header>
</template>

<style scoped>
.fade-enter-active,
.fade-leave-active {
  transition: opacity 120ms ease, transform 120ms ease;
}
.fade-enter-from,
.fade-leave-to {
  opacity: 0;
  transform: translateY(-3px);
}
</style>
