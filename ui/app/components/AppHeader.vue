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
  <header class="relative z-40 flex min-h-14 shrink-0 flex-wrap items-center gap-x-5 gap-y-2 border-b border-edge bg-well px-5 py-3">
    <div class="flex items-center gap-2">
      <img src="/logo.png" alt="" class="size-5" />
      <span class="text-[15px] font-semibold tracking-tight">Undo</span>
    </div>

    <div v-if="currentProject" class="relative min-w-0 border-l border-edge pl-5" @keydown.esc="menuOpen = false">
      <button
        v-if="state.projects.length > 1"
        class="flex max-w-[min(50vw,20rem)] items-center gap-2 text-[13px] text-mut hover:text-ink"
        :aria-expanded="menuOpen"
        aria-label="Switch project"
        @click="menuOpen = !menuOpen"
      >
        <UiIcon name="folder" :size="14" class="shrink-0" />
        <span class="truncate">{{ currentProject.name }}</span>
        <UiIcon name="chevron" :size="12" class="shrink-0" :class="menuOpen ? 'rotate-180' : ''" />
      </button>
      <span v-else class="flex max-w-[50vw] items-center gap-2 text-[13px] text-mut">
        <UiIcon name="folder" :size="14" class="shrink-0" />
        <span class="truncate">{{ currentProject.name }}</span>
      </span>
      <div
        v-if="menuOpen && state.projects.length > 1"
        class="absolute left-0 top-full z-40 mt-3 w-64 max-w-[calc(100vw-8rem)] overflow-hidden rounded-lg border border-edge bg-panel shadow-xl shadow-black/30"
      >
        <button
          v-for="project in state.projects"
          :key="project.id"
          class="flex w-full items-center gap-2.5 px-4 py-3 text-left text-[13px] hover:bg-well"
          :aria-current="project.id === state.projectId ? 'true' : undefined"
          @click="choose(project.id)"
        >
          <span class="size-1.5 shrink-0 rounded-full" :class="project.recording ? 'bg-live' : 'bg-dim'" />
          <span class="min-w-0 flex-1">
            <span class="block truncate text-ink">{{ project.name }}</span>
            <span class="block truncate text-[11px] text-mut" :title="project.root_path">{{ project.root_path }}</span>
          </span>
          <UiIcon v-if="project.id === state.projectId" name="check" :size="13" class="shrink-0 text-mut" />
        </button>
      </div>
    </div>

    <div class="ml-auto flex items-center gap-5">
      <span v-if="state.activeRuns.length > 0" class="hidden text-[12px] text-mut sm:inline" :title="state.activeRuns.map(run => run.label).join(', ')">
        {{ state.activeRuns.length }} active run{{ state.activeRuns.length === 1 ? '' : 's' }}
      </span>
      <span class="flex items-center gap-2 text-[12px]" :class="state.recording ? 'text-live' : 'text-warn'" role="status">
        <span class="size-1.5 shrink-0 rounded-full bg-current" />
        {{ state.recording ? 'Protected' : 'Not recording' }}
      </span>
    </div>
  </header>
</template>
