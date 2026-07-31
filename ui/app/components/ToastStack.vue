<script setup lang="ts">
const { state } = useUndo()
</script>

<template>
  <Teleport to="body">
    <div class="pointer-events-none fixed bottom-5 right-5 z-[60] flex w-80 flex-col gap-2">
      <TransitionGroup name="toast">
        <div
          v-for="toast in state.toasts"
          :key="toast.id"
          class="pointer-events-auto flex items-start gap-2.5 rounded-xl border bg-panel px-3.5 py-3 shadow-xl shadow-black/50"
          :class="toast.kind === 'ok' ? 'border-live/30' : 'border-del/30'"
        >
          <UiIcon
            :name="toast.kind === 'ok' ? 'check' : 'warning'"
            :size="14"
            class="mt-0.5 shrink-0"
            :class="toast.kind === 'ok' ? 'text-live' : 'text-del'"
          />
          <p class="text-[12.5px] leading-snug text-ink">{{ toast.message }}</p>
        </div>
      </TransitionGroup>
    </div>
  </Teleport>
</template>

<style scoped>
.toast-enter-active,
.toast-leave-active {
  transition: all 200ms ease;
}
.toast-enter-from,
.toast-leave-to {
  opacity: 0;
  transform: translateX(12px);
}
</style>
