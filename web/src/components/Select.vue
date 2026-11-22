<script setup lang="ts">
/** Native select with a real label; option copy comes from i18n. */
import { useId } from 'vue'

export interface SelectOption {
  value: string
  label: string
}

withDefaults(
  defineProps<{
    modelValue: string
    label: string
    options: SelectOption[]
    hint?: string
    disabled?: boolean
  }>(),
  { hint: '', disabled: false },
)

const emit = defineEmits<{ 'update:modelValue': [value: string] }>()
const id = useId()
</script>

<template>
  <div class="select" :class="{ 'select--disabled': disabled }">
    <label class="select__label" :for="id">{{ label }}</label>
    <p v-if="hint" class="select__hint">{{ hint }}</p>
    <select
      :id="id"
      class="select__input"
      :value="modelValue"
      :disabled="disabled"
      @change="emit('update:modelValue', ($event.target as HTMLSelectElement).value)"
    >
      <option v-for="option in options" :key="option.value" :value="option.value">
        {{ option.label }}
      </option>
    </select>
  </div>
</template>

<style scoped>
.select {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  min-width: 0;
}

.select--disabled {
  opacity: 0.55;
}

.select__label {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.select__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.select__input {
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.22rem var(--sp-2);
  width: 100%;
  min-width: 9rem;
  max-width: 100%;
}
</style>
