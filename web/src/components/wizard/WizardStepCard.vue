<script setup lang="ts">
/** One step's frame: number, title, done/pending chip, summary, body slot. */
import { t } from '../../i18n'
import Pill from '../Pill.vue'

withDefaults(
  defineProps<{
    index: number
    title: string
    done: boolean
    summary: string
    /** Configured but not verified in this session: a third state, so a
     * working service never reads as "待配置" (user report 2026-11-14). */
    saved?: boolean
  }>(),
  { saved: false },
)
</script>

<template>
  <section class="step">
    <header class="step__head">
      <span class="step__num" aria-hidden="true">{{ index }}</span>
      <div class="step__text">
        <h2 class="step__title">{{ title }}</h2>
        <p class="step__summary">{{ summary }}</p>
      </div>
      <Pill :tone="done ? 'ok' : 'warn'" size="sm">
        {{ done ? t('setupDone') : saved ? t('setupSavedState') : t('setupPendingState') }}
      </Pill>
    </header>
    <div class="step__body">
      <slot />
    </div>
  </section>
</template>

<style scoped>
.step {
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--r-md);
  box-shadow: var(--shadow-card);
  padding: var(--sp-4);
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.step__head {
  display: flex;
  align-items: flex-start;
  gap: var(--sp-3);
}

.step__num {
  flex: none;
  width: 1.5rem;
  height: 1.5rem;
  border-radius: 50%;
  background: var(--accent);
  color: var(--on-accent);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-family: var(--font-mono);
  font-size: var(--fs-xs);
  font-weight: 700;
}

.step__text {
  flex: 1;
  min-width: 0;
}

.step__title {
  font-size: var(--fs-md);
}

.step__summary {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.step__body {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}
</style>
