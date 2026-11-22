<script setup lang="ts">
/**
 * A status chip. Tone is semantic, never a per-call colour; `removable`
 * turns it into an editable phrase chip with an accessible remove button.
 */
import { t } from '../i18n'
import AppIcon from './AppIcon.vue'

withDefaults(
  defineProps<{
    tone?: 'neutral' | 'muted' | 'ok' | 'warn' | 'danger' | 'info' | 'live' | 'accent'
    size?: 'sm' | 'md'
    mono?: boolean
    removable?: boolean
    removeLabel?: string
  }>(),
  {
    tone: 'neutral',
    size: 'md',
    mono: false,
    removable: false,
    removeLabel: '',
  },
)

const emit = defineEmits<{ remove: [] }>()
</script>

<template>
  <span class="pill" :class="[`pill--${tone}`, `pill--${size}`, { 'pill--mono': mono }]">
    <slot />
    <button
      v-if="removable"
      type="button"
      class="pill__remove"
      :aria-label="removeLabel || t('remove')"
      @click="emit('remove')"
    >
      <AppIcon name="remove" :size="12" />
    </button>
  </span>
</template>

<style scoped>
.pill {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border-radius: var(--r-pill);
  border: 1px solid var(--line);
  background: var(--surface-2);
  color: var(--ink-2);
  padding: 0.08rem var(--sp-2);
  font-size: var(--fs-sm);
  line-height: 1.5;
  white-space: nowrap;
  max-width: 100%;
}

.pill--sm {
  font-size: var(--fs-xs);
  padding: 0 var(--sp-2);
}

.pill--muted {
  color: var(--muted);
}

.pill--mono {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
}

.pill--ok {
  background: var(--ok-soft);
  border-color: var(--ok-line);
  color: var(--ok);
}

.pill--warn {
  background: var(--warn-soft);
  border-color: var(--warn-line);
  color: var(--warn);
}

.pill--danger {
  background: var(--danger-soft);
  border-color: var(--danger-line);
  color: var(--danger);
}

.pill--info {
  background: var(--info-soft);
  border-color: var(--info-line);
  color: var(--info);
}

.pill--live {
  background: var(--live-soft);
  border-color: var(--live-line);
  color: var(--live);
}

.pill--accent {
  background: var(--accent-soft);
  border-color: var(--accent-line);
  color: var(--accent);
}

.pill__remove {
  border: 0;
  background: none;
  color: inherit;
  cursor: pointer;
  font-size: var(--fs-md);
  line-height: 1;
  padding: 0 0 0 var(--sp-1);
  opacity: 0.7;
}

.pill__remove:hover {
  opacity: 1;
  color: var(--danger);
}
</style>
