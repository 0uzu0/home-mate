<script setup lang="ts">
/** Text input with a real label, an optional hint, and Enter-to-submit. */
import { useId } from 'vue'

withDefaults(
  defineProps<{
    modelValue: string
    label: string
    hint?: string
    placeholder?: string
    type?: 'text' | 'password' | 'search'
    maxlength?: number
    disabled?: boolean
    autocomplete?: string
    list?: string
    wide?: boolean
  }>(),
  {
    hint: '',
    placeholder: '',
    type: 'text',
    maxlength: undefined,
    disabled: false,
    autocomplete: 'off',
    list: undefined,
    wide: false,
  },
)

const emit = defineEmits<{
  'update:modelValue': [value: string]
  submit: []
}>()
const id = useId()
</script>

<template>
  <div class="text" :class="{ 'text--disabled': disabled }">
    <label class="text__label" :for="id">{{ label }}</label>
    <p v-if="hint" class="text__hint">{{ hint }}</p>
    <input
      :id="id"
      class="text__input"
      :class="{ 'text__input--wide': wide }"
      :type="type"
      :value="modelValue"
      :placeholder="placeholder"
      :maxlength="maxlength"
      :disabled="disabled"
      :autocomplete="autocomplete"
      :list="list"
      @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
      @keydown.enter.prevent="emit('submit')"
    />
  </div>
</template>

<style scoped>
.text {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  min-width: 0;
}

.text--disabled {
  opacity: 0.55;
}

.text__label {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.text__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.text__input {
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.28rem var(--sp-2);
  width: 14rem;
  max-width: 100%;
}

.text__input--wide {
  width: 22rem;
}
</style>
