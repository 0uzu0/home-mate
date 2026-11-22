<script setup lang="ts">
/** Switch with its own label; the handler honours `disabled`, not just CSS. */
import { useId } from 'vue'

const props = withDefaults(
  defineProps<{
    modelValue: boolean
    label: string
    hint?: string
    disabled?: boolean
  }>(),
  { hint: '', disabled: false },
)

const emit = defineEmits<{ 'update:modelValue': [value: boolean] }>()
const id = useId()

function onChange(event: Event): void {
  if (props.disabled) return
  emit('update:modelValue', (event.target as HTMLInputElement).checked)
}
</script>

<template>
  <div class="toggle" :class="{ 'toggle--disabled': disabled }">
    <div class="toggle__text">
      <label class="toggle__label" :for="id">{{ label }}</label>
      <p v-if="hint" class="toggle__hint">{{ hint }}</p>
    </div>
    <input
      :id="id"
      class="toggle__input"
      type="checkbox"
      role="switch"
      :checked="modelValue"
      :disabled="disabled"
      @change="onChange"
    />
  </div>
</template>

<style scoped>
.toggle {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--sp-4);
  min-width: 0;
}

.toggle--disabled {
  opacity: 0.55;
}

.toggle__text {
  min-width: 0;
}

.toggle__label {
  font-size: var(--fs-base);
  color: var(--ink);
  cursor: pointer;
}

.toggle__hint {
  margin-top: var(--sp-1);
  font-size: var(--fs-sm);
  color: var(--muted);
  max-width: 60ch;
}

.toggle__input {
  appearance: none;
  flex: none;
  width: 2.4rem;
  height: 1.35rem;
  border-radius: var(--r-pill);
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  position: relative;
  cursor: pointer;
  transition: background var(--dur-1) var(--ease), border-color var(--dur-1) var(--ease);
  margin-top: 0.15rem;
}

.toggle__input::after {
  content: '';
  position: absolute;
  top: 2px;
  left: 2px;
  width: calc(1.35rem - 6px);
  height: calc(1.35rem - 6px);
  border-radius: 50%;
  background: var(--surface);
  box-shadow: var(--shadow-card);
  transition: transform var(--dur-1) var(--ease);
}

.toggle__input:checked {
  background: var(--accent);
  border-color: var(--accent);
}

.toggle__input:checked::after {
  transform: translateX(calc(2.4rem - 1.35rem));
}

.toggle__input:disabled {
  cursor: not-allowed;
}
</style>
