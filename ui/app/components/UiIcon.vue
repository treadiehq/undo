<script setup lang="ts">
// Hand-inlined 24px stroke icons (lucide-style) so the binary ships zero
// icon dependencies.
const props = defineProps<{ name: string; size?: number }>()

const paths: Record<string, string> = {
  // undo / restore
  undo: 'M3 7v6h6 M3 13a9 9 0 1 0 3-7.7L3 8',
  bot: 'M12 8V4 M8 4h8 M5 8h14a1 1 0 0 1 1 1v9a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V9a1 1 0 0 1 1-1Z M9 13v2 M15 13v2',
  pencil:
    'M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z M15 5l4 4',
  terminal: 'M4 17l6-6-6-6 M12 19h8',
  zap: 'M13 2 3 14h9l-1 8 10-12h-9l1-8Z',
  file: 'M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8Z M14 2v6h6',
  trash:
    'M3 6h18 M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6 M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2',
  chevron: 'M6 9l6 6 6-6',
  warning:
    'M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z M12 9v4 M12 17h.01',
  x: 'M18 6 6 18 M6 6l12 12',
  check: 'M20 6 9 17l-5-5',
  clock: 'M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20Z M12 6v6l4 2',
  flag: 'M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1Z M4 22v-7',
  folder:
    'M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z',
  sparkle:
    'M9.9 2.5a1 1 0 0 1 1.9 0l1.3 3.8a1 1 0 0 0 .6.6l3.8 1.3a1 1 0 0 1 0 1.9l-3.8 1.3a1 1 0 0 0-.6.6l-1.3 3.8a1 1 0 0 1-1.9 0L8.6 12a1 1 0 0 0-.6-.6L4.2 10a1 1 0 0 1 0-1.9L8 6.9a1 1 0 0 0 .6-.6Z M18 14l.9 2.6 2.6.9-2.6.9L18 21l-.9-2.6-2.6-.9 2.6-.9Z',
  plusminus: 'M5 9h6 M8 6v6 M13 15h6',
  search: 'M11 19a8 8 0 1 0 0-16 8 8 0 0 0 0 16Z M21 21l-4.3-4.3',
}
</script>

<template>
  <svg
    :width="props.size ?? 16"
    :height="props.size ?? 16"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path :d="paths[props.name] ?? ''" />
  </svg>
</template>
