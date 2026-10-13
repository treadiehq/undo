<script setup lang="ts">
// Measure only entering/leaving rows. Existing rows keep their DOM and scroll
// position; natural height changes carry their neighbours into place.
function measure(element: Element) {
  const row = element as HTMLElement
  row.style.setProperty('--collapse-height', `${row.scrollHeight}px`)
}

function clear(element: Element) {
  const row = element as HTMLElement
  row.style.removeProperty('--collapse-height')
  row.removeAttribute('inert')
  row.removeAttribute('aria-hidden')
}

function beforeLeave(element: Element) {
  measure(element)
  element.setAttribute('inert', '')
  element.setAttribute('aria-hidden', 'true')
}
</script>

<template>
  <TransitionGroup
    tag="div"
    name="collapse"
    @enter="measure"
    @before-leave="beforeLeave"
    @after-enter="clear"
    @enter-cancelled="clear"
    @leave-cancelled="clear"
  ><slot /></TransitionGroup>
</template>
