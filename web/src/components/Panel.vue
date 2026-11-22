<script setup lang="ts">
/**
 * The one card primitive: icon, title, optional hint, an apply badge, an
 * actions slot, body.
 *
 * `apply` says what happens to a save: immediate, next turn, or a restart.
 * It is a property of the card, not of a particular button, so the console
 * cannot end up with two cards disagreeing about the same API.
 */
import { computed } from 'vue'
import { t, type Key } from '../i18n'
import AppIcon from './AppIcon.vue'
import Pill from './Pill.vue'
import type { IconName } from './icons'

export type ApplyMode = 'immediate' | 'next-turn' | 'restart'

const props = withDefaults(
  defineProps<{
    title: string
    hint?: string
    icon?: IconName
    apply?: ApplyMode
    tone?: 'default' | 'ok' | 'warn' | 'danger'
    dense?: boolean
  }>(),
  { hint: '', icon: undefined, apply: undefined, tone: 'default', dense: false },
)

const applyMeta = computed<{
  tone: 'ok' | 'info' | 'warn'
  icon: IconName
  label: Key
  hint: Key
} | null>(() => {
  switch (props.apply) {
    case 'immediate':
      return { tone: 'ok', icon: 'check', label: 'applyImmediate', hint: 'applyImmediateHint' }
    case 'next-turn':
      return { tone: 'info', icon: 'timer', label: 'applyNextTurn', hint: 'applyNextTurnHint' }
    case 'restart':
      return { tone: 'warn', icon: 'power', label: 'applyRestart', hint: 'applyRestartHint' }
    default:
      return null
  }
})
</script>

<template>
  <section class="panel" :class="[`panel--${tone}`, { 'panel--dense': dense }]">
    <header class="panel__head">
      <div class="panel__headings">
        <div class="panel__title-row">
          <AppIcon v-if="icon" :name="icon" :size="18" class="panel__icon" />
          <h2 class="panel__title">{{ title }}</h2>
        </div>
        <p v-if="hint" class="panel__hint">{{ hint }}</p>
      </div>
      <div v-if="$slots.actions || applyMeta" class="panel__actions">
        <Pill
          v-if="applyMeta"
          :tone="applyMeta.tone"
          size="sm"
          mono
          :title="t(applyMeta.hint)"
        >
          <AppIcon :name="applyMeta.icon" :size="11" />
          {{ t(applyMeta.label) }}
        </Pill>
        <slot name="actions" />
      </div>
    </header>
    <div class="panel__body">
      <slot />
    </div>
  </section>
</template>

<style scoped>
.panel {
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--r-md);
  box-shadow: var(--shadow-card);
  padding: var(--sp-4) var(--sp-4) var(--sp-4);
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  min-width: 0;
}

.panel--dense {
  padding: var(--sp-3);
  gap: var(--sp-2);
}

.panel--ok {
  border-left: 3px solid var(--ok);
}

.panel--warn {
  border-left: 3px solid var(--warn);
}

.panel--danger {
  border-left: 3px solid var(--danger);
}

.panel__head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.panel__headings {
  min-width: 0;
}

.panel__title-row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  min-width: 0;
}

.panel__icon {
  color: var(--accent);
}

.panel__title {
  font-size: var(--fs-md);
  font-weight: 650;
  color: var(--ink);
}

.panel__hint {
  margin-top: var(--sp-1);
  color: var(--muted);
  font-size: var(--fs-sm);
  max-width: 60ch;
}

.panel__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.panel__body {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  min-width: 0;
}
</style>
