<script setup lang="ts">
/** Loading placeholder with the same footprint as the content it replaces. */
import AppIcon from './AppIcon.vue'

withDefaults(defineProps<{ label?: string; rows?: number }>(), { label: '', rows: 3 })
</script>

<template>
  <div class="loading" role="status" aria-live="polite">
    <span v-if="label" class="loading__label">
      <AppIcon name="loading" :size="14" class="loading__spinner" />
      {{ label }}
    </span>
    <span v-for="row in rows" :key="row" class="loading__row" aria-hidden="true" />
  </div>
</template>

<style scoped>
.loading {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-2) 0;
}

.loading__label {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  font-size: var(--fs-sm);
  color: var(--muted);
}

.loading__spinner {
  animation: spin 1.1s linear infinite;
}

@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}

.loading__row {
  height: 0.7rem;
  border-radius: var(--r-xs);
  background: linear-gradient(
    90deg,
    var(--surface-2) 0%,
    var(--surface-3) 50%,
    var(--surface-2) 100%
  );
  background-size: 200% 100%;
  animation: shimmer 1.6s linear infinite;
}

.loading__row:nth-child(2) {
  width: 88%;
}

.loading__row:nth-child(3) {
  width: 72%;
}

@keyframes shimmer {
  from {
    background-position: 200% 0;
  }
  to {
    background-position: -200% 0;
  }
}
</style>
