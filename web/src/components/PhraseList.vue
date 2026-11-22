<script setup lang="ts">
/**
 * Editable list of short phrases (wake words, stop words, cue phrases).
 * One component because the add/remove behaviour and its a11y must not drift
 * between six cards that all need the same thing.
 */
import { ref, useId } from 'vue'
import { t } from '../i18n'
import Pill from './Pill.vue'
import EmptyState from './EmptyState.vue'

const props = withDefaults(
  defineProps<{
    modelValue: string[]
    label: string
    hint?: string
    placeholder?: string
    addLabel?: string
    emptyLabel?: string
    maxlength?: number
    disabled?: boolean
  }>(),
  {
    hint: '',
    placeholder: '',
    addLabel: '',
    emptyLabel: '',
    maxlength: 40,
    disabled: false,
  },
)

const emit = defineEmits<{ 'update:modelValue': [value: string[]] }>()
const id = useId()
const draft = ref('')

function add(): void {
  if (props.disabled) return
  const value = draft.value.trim()
  if (!value) return
  if (!props.modelValue.includes(value)) {
    emit('update:modelValue', [...props.modelValue, value])
  }
  draft.value = ''
}

function remove(value: string): void {
  if (props.disabled) return
  emit(
    'update:modelValue',
    props.modelValue.filter((item) => item !== value),
  )
}
</script>

<template>
  <div class="phrases">
    <p class="phrases__label">{{ label }}</p>
    <p v-if="hint" class="phrases__hint">{{ hint }}</p>
    <div v-if="modelValue.length" class="phrases__chips">
      <Pill
        v-for="item in modelValue"
        :key="item"
        removable
        :remove-label="t('removeItem', { item })"
        @remove="remove(item)"
      >
        {{ item }}
      </Pill>
    </div>
    <EmptyState v-else :title="emptyLabel || t('empty')" />
    <div class="phrases__row">
      <label class="sr-only" :for="id">{{ label }}</label>
      <input
        :id="id"
        v-model="draft"
        class="phrases__input"
        type="text"
        :placeholder="placeholder"
        :maxlength="maxlength"
        :disabled="disabled"
        @keydown.enter.prevent="add"
      />
      <button type="button" class="btn btn--sm" :disabled="disabled || !draft.trim()" @click="add">
        {{ addLabel || t('add') }}
      </button>
    </div>
  </div>
</template>

<style scoped>
.phrases {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  min-width: 0;
}

.phrases__label {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.phrases__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.phrases__chips {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-1);
}

.phrases__row {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
  align-items: center;
}

.phrases__input {
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.24rem var(--sp-2);
  width: 14rem;
  max-width: 100%;
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  cursor: pointer;
}

.btn--sm {
  padding: 0.2rem var(--sp-3);
  font-size: var(--fs-sm);
}

.btn:hover:not(:disabled) {
  background: var(--surface-3);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
