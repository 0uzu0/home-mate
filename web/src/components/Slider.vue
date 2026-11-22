<script setup lang="ts">
/**
 * Range control with a live value readout. Used where the value is tuned by
 * ear (volume, trim, protection window); exact entry uses NumberField.
 */
import { useId } from 'vue'

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
    format?: (value: number) => string
  }>(),
  { step: 1, unit: '', hint: '', disabled: false, format: undefined },
)

const emit = defineEmits<{ 'update:modelValue': [value: number] }>()
const id = useId()

function display(value: number): string {
  return props.format ? props.format(value) : `${value}${props.unit ? ` ${props.unit}` : ''}`
}
</script>

<template>
  <div class="slider" :class="{ 'slider--disabled': disabled }">
    <div class="slider__head">
      <div class="slider__text">
        <label class="slider__label" :for="id">{{ label }}</label>
        <p v-if="hint" class="slider__hint">{{ hint }}</p>
      </div>
      <output class="slider__value mono" :for="id">{{ display(modelValue) }}</output>
    </div>
    <input
      :id="id"
      class="slider__input"
      type="range"
      :min="min"
      :max="max"
      :step="step"
      :value="modelValue"
      :disabled="disabled"
      :aria-valuetext="display(modelValue)"
      @input="emit('update:modelValue', Number(($event.target as HTMLInputElement).value))"
    />
  </div>
</template>

<style scoped>
.slider {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  min-width: 0;
}

.slider--disabled {
  opacity: 0.55;
}

.slider__head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--sp-3);
}

.slider__text {
  min-width: 0;
}

.slider__label {
  font-size: var(--fs-base);
  color: var(--ink);
}

.slider__hint {
  margin-top: var(--sp-1);
  font-size: var(--fs-sm);
  color: var(--muted);
  max-width: 60ch;
}

.slider__value {
  flex: none;
  font-size: var(--fs-sm);
  color: var(--ink-2);
  background: var(--surface-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  padding: 0 var(--sp-2);
}

.slider__input {
  width: 100%;
  accent-color: var(--accent);
  cursor: pointer;
}
</style>
