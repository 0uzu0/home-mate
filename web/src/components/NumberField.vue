<script setup lang="ts">
/**
 * Exact numeric entry (thresholds, milliseconds, dB). Defaults to integer-only;
 * passing a fractional `step` enables decimal entry with clean precision
 * rounding on blur. Mid-keystroke emptiness is allowed but never committed, and
 * blur snaps the box back to what is actually saved.
 */
import { computed, ref, useId, watch } from 'vue'
import { clamp } from '../format'

const props = withDefaults(
  defineProps<{
    modelValue: number
    label: string
    min: number
    max: number
    step?: number
    unit?: string
    hint?: string
    disabled?: boolean
    placeholder?: string
  }>(),
  { step: undefined, unit: '', hint: '', disabled: false, placeholder: '' },
)

const emit = defineEmits<{ 'update:modelValue': [value: number] }>()
const id = useId()
const text = ref(String(props.modelValue))
const editing = ref(false)

const isDecimal = computed(() => props.step !== undefined && props.step > 0 && !Number.isInteger(props.step))

function stepDecimals(step: number): number {
  const s = step.toString().toLowerCase()
  if (s.includes('e-')) {
    const [base, exp] = s.split('e-')
    const frac = base.includes('.') ? base.split('.')[1].length : 0
    return Number(exp) + frac
  }
  return s.includes('.') ? s.split('.')[1].length : 0
}

function normalize(value: number): number {
  if (!isDecimal.value) return Math.trunc(value)
  const factor = 10 ** stepDecimals(props.step!)
  return Math.round(value * factor) / factor
}

watch(
  () => props.modelValue,
  (value) => {
    if (!editing.value) text.value = String(value)
  },
)

function commit(raw: string): void {
  if (props.disabled) return
  if (raw === '' || raw === '-' || raw === '.' || raw === '-.' || raw.endsWith('.')) return
  const parsed = Number(raw)
  if (!Number.isFinite(parsed)) return
  if (!isDecimal.value && !Number.isInteger(parsed)) return
  emit('update:modelValue', clamp(parsed, props.min, props.max))
}

function onInput(event: Event): void {
  const raw = (event.target as HTMLInputElement).value
  const pattern = isDecimal.value ? /^-?\d*\.?\d*$/ : /^-?\d*$/
  if (!pattern.test(raw)) return
  text.value = raw
  commit(raw)
}

function onBlur(): void {
  editing.value = false
  const parsed = Number(text.value)
  const next =
    text.value === '' || !Number.isFinite(parsed)
      ? props.modelValue
      : clamp(normalize(parsed), props.min, props.max)
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
        :inputmode="isDecimal ? 'decimal' : 'numeric'"
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
