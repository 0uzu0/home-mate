<script setup lang="ts">
/**
 * One scalar readout: label, value, unit, optional note. Values are strings
 * already formatted by the caller, so the tile never guesses.
 */
withDefaults(
  defineProps<{
    label: string
    value: string | number | null | undefined
    unit?: string
    tone?: 'default' | 'ok' | 'warn' | 'danger' | 'info' | 'live' | 'accent' | 'muted'
    note?: string
    hint?: string
  }>(),
  { unit: '', tone: 'default', note: '', hint: '' },
)
</script>

<template>
  <div class="tile" :class="`tile--${tone}`" :title="hint || undefined">
    <span class="tile__label">{{ label }}</span>
    <span class="tile__value mono">
      {{ value === null || value === undefined || value === '' ? '—' : value
      }}<span v-if="unit && value !== null && value !== undefined && value !== ''" class="tile__unit">{{
        unit
      }}</span>
    </span>
    <span v-if="note" class="tile__note">{{ note }}</span>
  </div>
</template>

<style scoped>
.tile {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  min-width: 0;
}

.tile__label {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}

.tile__value {
  font-size: var(--fs-lg);
  color: var(--ink);
  word-break: break-word;
}

.tile__unit {
  font-size: var(--fs-sm);
  color: var(--muted);
  margin-left: 0.15em;
}

.tile__note {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.tile--ok .tile__value {
  color: var(--ok);
}

.tile--warn .tile__value {
  color: var(--warn);
}

.tile--danger .tile__value {
  color: var(--danger);
}

.tile--info .tile__value {
  color: var(--info);
}

.tile--live .tile__value {
  color: var(--live);
}

.tile--accent .tile__value {
  color: var(--accent);
}

.tile--muted .tile__value {
  color: var(--muted);
}
</style>
