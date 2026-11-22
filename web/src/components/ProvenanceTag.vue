<script setup lang="ts">
/**
 * Where a value came from. The literal id stays visible (operators grep for
 * it); the translated explanation lives in the tooltip.
 */
withDefaults(
  defineProps<{ source: string; label?: string; description?: string }>(),
  { label: '', description: '' },
)
</script>

<template>
  <span class="tag" :title="description || source">
    <span class="tag__dot" aria-hidden="true" />
    <span class="tag__text mono">{{ label || source }}</span>
  </span>
</template>

<style scoped>
.tag {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-pill);
  background: var(--surface-2);
  color: var(--muted);
  padding: 0 var(--sp-2);
  font-size: var(--fs-xs);
  max-width: 100%;
}

.tag__dot {
  width: 0.4rem;
  height: 0.4rem;
  border-radius: 50%;
  background: var(--accent);
  flex: none;
}

.tag__text {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
