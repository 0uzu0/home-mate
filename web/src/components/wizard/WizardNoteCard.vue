<script setup lang="ts">
/** A step that is just an honest note (tools are registry-rendered; HA is not integrated). */
import { t } from '../../i18n'
import Pill from '../Pill.vue'

withDefaults(defineProps<{ hint: string; done?: boolean; doneLabel?: string }>(), {
  done: true,
  doneLabel: '',
})
</script>

<template>
  <div class="note">
    <Pill :tone="done ? 'ok' : 'warn'" size="sm">
      {{ done ? doneLabel || t('setupDone') : t('setupPendingState') }}
    </Pill>
    <p class="note__hint">{{ hint }}</p>
  </div>
</template>

<style scoped>
.note {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  align-items: flex-start;
}

.note__hint {
  font-size: var(--fs-sm);
  color: var(--muted);
  max-width: 70ch;
}
</style>
