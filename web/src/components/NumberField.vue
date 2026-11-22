<script setup lang="ts">
/**
 * Exact integer entry (thresholds, milliseconds, dB). Today's value is the
 * stored value: mid-keystroke emptiness is allowed but never committed, and
 * blur snaps the box back to what is actually saved.
 */
import { ref, useId, watch } from 'vue'
import { clamp } from '../format'

const props = withDefaults(
  defineProps<{
    modelValue: number
    label: string
    min: number
    max: number
    unit?: string
    hint?: string
    disabled?: boolean
    placeholder?: string
  }>(),
  { unit: '', hint: '', disabled: false, placeholder: '' },
)

const emit = defineEmits<{ 'update:modelValue': [value: number] }>()
const id = useId()
const text = ref(String(props.modelValue))
const editing = ref(false)

watch(
  () => props.modelValue,
  (value) => {
    if (!editing.value) text.value = String(value)
  },
)

function commit(raw: string): void {
  if (props.disabled) return
  if (raw === '' || raw === '-') return
  const parsed = Number(raw)
  if (!Number.isFinite(parsed) || !Number.isInteger(parsed)) return
  emit('update:modelValue', clamp(parsed, props.min, props.max))
}

function onInput(event: Event): void {
  const raw = (event.target as HTMLInputElement).value
  if (!/^-?\d*$/.test(raw)) return
  text.value = raw
  commit(raw)
}

function onBlur(): void {
  editing.value = false
  const parsed = Number(text.value)
  const next =
    text.value === '' || !Number.isFinite(parsed)
      ? props.modelValue
      : clamp(Math.trunc(parsed), props.min, props.max)
  text.value = String(next)
  if (next !== props.modelValue) emit('update:modelValue', next)
}
</script>

<template>
  <div class="field" :class="{ 'field--disabled': disabled }">
    <label class="field__label" :for="id">{{ label }}</label>
    <p v-if="hint" class="field__hint">{{ hint }}</p>
    <div class="field__row">
      <input
        :id="id"
        class="field__input mono"
        type="text"
        inputmode="numeric"
        autocomplete="off"
        :value="text"
        :disabled="disabled"
        :placeholder="placeholder"
        @focus="editing = true"
        @input="onInput"
        @blur="onBlur"
      />
      <span v-if="unit" class="field__unit mono">{{ unit }}</span>
    </div>
  </div>
</template>

<style scoped>
.field {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  min-width: 0;
}

.field--disabled {
  opacity: 0.55;
}

.field__label {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.field__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.field__row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
}

.field__input {
  width: 6.5rem;
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.22rem var(--sp-2);
}

.field__unit {
  font-size: var(--fs-sm);
  color: var(--muted);
}
</style>
