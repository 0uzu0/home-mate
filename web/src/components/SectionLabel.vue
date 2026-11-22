<script setup lang="ts">
/** Quiet uppercase label for a sub-section inside a panel. */
defineProps<{ text: string }>()
</script>

<template>
  <h3 class="section-label">{{ text }}</h3>
</template>

<style scoped>
.section-label {
  font-size: var(--fs-xs);
  font-weight: 650;
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}
</style>
