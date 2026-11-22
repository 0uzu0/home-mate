<script setup lang="ts">
/** Horizontal meter with an optional label row; `value` is 0–100 or null. */
import { computed } from 'vue'
import { clamp } from '../format'

const props = withDefaults(
  defineProps<{
    label?: string
    text?: string
    value: number | null
    tone?: 'accent' | 'ok' | 'warn' | 'danger' | 'muted'
  }>(),
  { label: '', text: '', tone: 'accent' },
)

const width = computed(() =>
  props.value === null ? 0 : clamp(Math.round(props.value), 0, 100),
)
</script>

<template>
  <div class="meter" :class="`meter--${tone}`">
    <div v-if="label || text" class="meter__row">
      <span v-if="label" class="meter__label">{{ label }}</span>
      <span v-if="text" class="meter__text mono">{{ text }}</span>
    </div>
    <div
      class="meter__track"
      role="meter"
      :aria-valuenow="value === null ? undefined : width"
      aria-valuemin="0"
      aria-valuemax="100"
      :aria-label="label || undefined"
    >
      <div v-if="value !== null" class="meter__fill" :style="{ width: `${width}%` }" />
    </div>
  </div>
</template>

<style scoped>
.meter {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  min-width: 0;
}

.meter__row {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: var(--sp-2);
}

.meter__label {
  font-size: var(--fs-xs);
  color: var(--muted);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
}

.meter__text {
  font-size: var(--fs-xs);
  color: var(--ink-2);
}

.meter__track {
  height: 4px;
  border-radius: var(--r-xs);
  background: var(--surface-inset);
  overflow: hidden;
}

.meter__fill {
  height: 100%;
  border-radius: var(--r-xs);
  background: var(--accent);
  transition: width var(--dur-2) var(--ease);
}

.meter--ok .meter__fill {
  background: var(--ok);
}

.meter--warn .meter__fill {
  background: var(--warn);
}

.meter--danger .meter__fill {
  background: var(--danger);
}

.meter--muted .meter__fill {
  background: var(--muted);
}
</style>
