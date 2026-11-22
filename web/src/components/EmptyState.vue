<script setup lang="ts">
/** Empty state: says what is missing and, when useful, how to fill it. */
import AppIcon from './AppIcon.vue'
import type { IconName } from './icons'

withDefaults(defineProps<{ title: string; hint?: string; icon?: IconName }>(), {
  hint: '',
  icon: 'empty',
})
</script>

<template>
  <div class="empty">
    <AppIcon :name="icon" :size="18" class="empty__icon" />
    <p class="empty__title">{{ title }}</p>
    <p v-if="hint" class="empty__hint">{{ hint }}</p>
    <slot />
  </div>
</template>

<style scoped>
.empty {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  align-items: flex-start;
  padding: var(--sp-4);
  border: 1px dashed var(--line-strong);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.empty__icon {
  color: var(--faint);
  margin-bottom: var(--sp-1);
}

.empty__title {
  color: var(--ink-2);
  font-size: var(--fs-base);
}

.empty__hint {
  color: var(--muted);
  font-size: var(--fs-sm);
}
</style>
