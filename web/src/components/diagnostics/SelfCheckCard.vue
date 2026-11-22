<script setup lang="ts">
/**
 * One-click self check (R2-BM6): mic → asr → model → playback → services.
 *
 * Two buttons, because the two operations are different:
 * - Quick: passive. Reads state, counters and config; ZERO outbound requests.
 * - Deep: runs the existing real probes (model ping, ASR sample, TTS
 *   synthesis, service probes). Still never plays a sound.
 * The stage words are the same ones the service cards use.
 */
import { computed, ref } from 'vue'
import { runSelfCheck, type SelfCheckResult } from '../../api'
import { errorDetail } from '../../format'
import { t } from '../../i18n'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import AppIcon from '../AppIcon.vue'

const result = ref<SelfCheckResult | null>(null)
const running = ref(false)
const error = ref('')

const verdictTone = computed(() => {
  switch (result.value?.verdict) {
    case 'ready':
      return 'ok' as const
    case 'failed':
      return 'danger' as const
    default:
      return 'warn' as const
  }
})

const verdictLabel = computed(() => {
  switch (result.value?.verdict) {
    case 'ready':
      return t('selfCheckVerdictReady')
    case 'failed':
      return t('selfCheckVerdictFailed')
    case 'degraded':
      return t('selfCheckVerdictDegraded')
    default:
      return '—'
  }
})

function stageLabel(stage: string): string {
  switch (stage) {
    case 'mic':
      return t('selfCheckStageMic')
    case 'asr':
      return t('selfCheckStageAsr')
    case 'model':
      return t('selfCheckStageModel')
    case 'playback':
      return t('selfCheckStagePlayback')
    case 'services':
      return t('selfCheckStageServices')
    default:
      return stage
  }
}

function stateLabel(state: string): string {
  switch (state) {
    case 'ready':
      return t('selfCheckStateReady')
    case 'no_device':
      return t('selfCheckStateNoDevice')
    case 'inactive':
      return t('selfCheckStateInactive')
    case 'stale':
      return t('selfCheckStateStale')
    case 'unconfigured':
      return t('selfCheckStateUnconfigured')
    case 'disabled':
      return t('selfCheckStateDisabled')
    case 'failed':
      return t('selfCheckStateFailed')
    default:
      return t('selfCheckStateUnknown')
  }
}

function stateTone(state: string): 'ok' | 'warn' | 'danger' | 'info' {
  if (state === 'ready') return 'ok'
  if (state === 'failed') return 'danger'
  if (state === 'unconfigured' || state === 'disabled') return 'info'
  return 'warn'
}

async function run(deep: boolean): Promise<void> {
  running.value = true
  error.value = ''
  try {
    result.value = await runSelfCheck(deep)
  } catch (err) {
    error.value = errorDetail(err)
  } finally {
    running.value = false
  }
}
</script>

<template>
  <Panel icon="probes" :title="t('selfCheckTitle')" :hint="t('selfCheckHint')">
    <template #actions>
      <Pill v-if="result" :tone="verdictTone" size="sm" mono>{{ verdictLabel }}</Pill>
    </template>

    <div class="sc__actions">
      <button type="button" class="btn btn--primary" :disabled="running" @click="run(false)">
        <AppIcon name="check" :size="13" />
        {{ running ? t('selfCheckRunning') : t('selfCheckQuick') }}
      </button>
      <button type="button" class="btn" :disabled="running" @click="run(true)">
        <AppIcon name="probes" :size="13" />
        {{ running ? t('selfCheckRunning') : t('selfCheckDeep') }}
      </button>
      <span class="sc__hint">{{ t('selfCheckDeepHint') }}</span>
    </div>

    <p v-if="error" class="sc__msg sc__msg--error">{{ error }}</p>

    <template v-if="result">
      <p class="sc__note">{{ result.note }}</p>
      <p v-if="result.verdict_stage" class="sc__first">
        {{ t('selfCheckFirstStage', { stage: stageLabel(result.verdict_stage) }) }}
      </p>
      <ul class="sc__stages">
        <li v-for="stage in result.stages" :key="stage.stage" class="sc__stage">
          <div class="sc__stage-head">
            <span class="sc__stage-name">{{ stageLabel(stage.stage) }}</span>
            <Pill :tone="stateTone(stage.state)" size="sm" mono>{{ stateLabel(stage.state) }}</Pill>
          </div>
          <p class="sc__detail">{{ stage.detail }}</p>
          <details class="sc__evidence">
            <summary>{{ t('selfCheckEvidence') }}</summary>
            <pre class="sc__json">{{ JSON.stringify(stage.evidence, null, 2) }}</pre>
          </details>
        </li>
      </ul>
    </template>
  </Panel>
</template>

<style scoped>
.sc__actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
}

.sc__hint,
.sc__note,
.sc__detail {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.sc__first {
  font-size: var(--fs-sm);
  color: var(--ink-2);
}

.sc__msg {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.sc__msg--error {
  color: var(--danger);
}

.sc__stages {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  margin: 0;
  padding: 0;
  list-style: none;
}

.sc__stage {
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.sc__stage-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-2);
}

.sc__stage-name {
  font-weight: 650;
  color: var(--ink);
}

.sc__evidence {
  margin-top: var(--sp-1);
  font-size: var(--fs-xs);
  color: var(--muted);
}

.sc__json {
  max-height: 14rem;
  overflow: auto;
  margin: var(--sp-1) 0 0;
  padding: var(--sp-2);
  border-radius: var(--r-sm);
  background: var(--surface);
  font-size: var(--fs-xs);
}
</style>
