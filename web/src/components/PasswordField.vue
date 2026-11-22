<script setup lang="ts">
/**
 * Write-only secret. The stored value is never sent to the browser, so this
 * field shows only whether one exists; typing supplies a replacement and an
 * empty box means "leave it unchanged".
 */
import { useId } from 'vue'
import { t } from '../i18n'

withDefaults(
  defineProps<{
    modelValue: string
    label: string
    isSet: boolean
    hint?: string
    placeholder?: string
    disabled?: boolean
    autocomplete?: string
  }>(),
  { hint: '', placeholder: '', disabled: false, autocomplete: 'new-password' },
)

const emit = defineEmits<{ 'update:modelValue': [value: string] }>()
const id = useId()
</script>

<template>
  <div class="secret" :class="{ 'secret--disabled': disabled }">
    <div class="secret__head">
      <label class="secret__label" :for="id">{{ label }}</label>
      <span class="secret__state" :class="isSet ? 'secret__state--set' : ''">
        {{ isSet ? t('apiKeySet') : t('apiKeyUnset') }}
      </span>
    </div>
    <p v-if="hint" class="secret__hint">{{ hint }}</p>
    <input
      :id="id"
      class="secret__input"
      type="password"
      :value="modelValue"
      :disabled="disabled"
      :placeholder="placeholder || t('apiKeyKeep')"
      :autocomplete="autocomplete"
      @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
    />
  </div>
</template>

<style scoped>
.secret {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  min-width: 0;
}

.secret--disabled {
  opacity: 0.55;
}

.secret__head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: var(--sp-2);
}

.secret__label {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.secret__state {
  font-size: var(--fs-xs);
  color: var(--warn);
}

.secret__state--set {
  color: var(--ok);
}

.secret__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.secret__input {
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.22rem var(--sp-2);
  width: 100%;
  max-width: 24rem;
}
</style>
