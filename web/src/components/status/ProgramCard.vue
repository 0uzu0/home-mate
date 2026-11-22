<script setup lang="ts">
/**
 * The spoken program: what is being read, where Continue would pick up, and
 * the page entry for pause/resume. This is the entry that does not cross the
 * microphone or the barge-in guard.
 */
import { computed } from 'vue'
import { pauseProgram, resumeProgram, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import Meter from '../Meter.vue'
import LedDot from '../LedDot.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const { busy, message, tone, run } = useSave()

const state = computed(() => {
  const program = props.device?.program
  if (program?.paused) {
    return { kind: 'paused' as const, program: program.paused }
  }
  if (program?.live) {
    return { kind: 'live' as const, program: program.live }
  }
  return { kind: 'idle' as const, program: null }
})

const pct = computed(() => {
  const program = state.value.program
  if (!program || !program.chars) return 0
  return Math.round((program.cursor / program.chars) * 100)
})

async function pause(): Promise<void> {
  if (!props.device) return
  await run(async () => {
    const result = await pauseProgram(props.device!.device)
    await props.refresh()
    return result.paused ? undefined : t('programNone')
  })
}

async function resume(): Promise<void> {
  if (!props.device) return
  await run(async () => {
    const result = await resumeProgram(props.device!.device)
    await props.refresh()
    return result.resumed ? undefined : t('programNone')
  })
}
</script>

<template>
  <Panel
    icon="program" :title="t('programTitle')" :hint="t('programHint')">
    <template #actions>
      <Pill :tone="state.kind === 'live' ? 'live' : state.kind === 'paused' ? 'warn' : 'neutral'" size="sm">
        <LedDot
          v-if="state.kind !== 'idle'"
          :state="state.kind === 'paused' ? 'paused' : 'speaking'"
          size="sm"
          :title="state.kind === 'paused' ? t('programPaused') : t('programLive')"
        />
        {{ state.kind === 'live' ? t('programLive') : state.kind === 'paused' ? t('programPaused') : t('programIdle') }}
      </Pill>
    </template>

    <EmptyState v-if="state.kind === 'idle'" :title="t('programIdle')" />

    <template v-else-if="state.program">
      <div class="program__row">
        <Meter
          :label="t('programCursor')"
          :text="`${pct}% · ${state.program.cursor}/${state.program.chars}`"
          :value="pct"
          :tone="state.kind === 'paused' ? 'warn' : 'accent'"
        />
        <Pill :tone="state.program.complete ? 'ok' : 'info'" size="sm">
          {{ state.program.complete ? t('programComplete') : t('programTruncated') }}
        </Pill>
      </div>

      <div v-if="state.kind === 'paused'" class="program__next">
        <span class="program__next-label">{{ t('programNext') }}</span>
        <p class="program__next-text">{{ state.program.next }}…</p>
        <span v-if="state.program.since_ms" class="program__since mono">
          {{ t('programPausedSince', { time: formatTime(state.program.since_ms) }) }}
        </span>
      </div>

      <div class="program__actions">
        <button type="button" class="btn" :disabled="busy" @click="pause">
          <AppIcon name="pause" :size="13" /> {{ t('programPause') }}
        </button>
        <button type="button" class="btn btn--primary" :disabled="busy" @click="resume">
          <AppIcon name="play" :size="13" /> {{ t('programResume') }}
        </button>
        <span v-if="message" class="program__msg" :class="`program__msg--${tone}`">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.program__row {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.program__row > :first-child {
  flex: 1 1 16rem;
}

.program__next {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.program__next-label {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}

.program__next-text {
  color: var(--ink-2);
}

.program__since {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.program__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.28rem var(--sp-4);
  cursor: pointer;
}

.btn--primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.program__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.program__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
