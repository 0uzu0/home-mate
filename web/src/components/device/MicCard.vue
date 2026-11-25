<script setup lang="ts">
/**
 * Microphone telemetry, read-only: session, flow, levels, lifetime counters
 * and latency. Nothing here is inferred — a missing report says so.
 */
import { computed } from 'vue'
import { isDeviceIdleGated, type GatewayDevice, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { formatDb, formatDuration } from '../../format'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import StatTile from '../StatTile.vue'
import Meter from '../Meter.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  device: ModeDevice | null
  gatewayDevice?: GatewayDevice | null
}>()

const mic = computed(() => props.device?.mic ?? null)
const idleGated = computed(() => isDeviceIdleGated(props.device, props.gatewayDevice))

const sessionLabel = computed(() => {
  if (mic.value?.session === true) return t('micSessionOn')
  if (mic.value?.session === false) return t('micSessionOff')
  return t('micSessionUnknown')
})

/** dBFS to a 0–100 bar over the useful range (−60…0). */
function levelPct(db: number | null | undefined): number | null {
  if (db === null || db === undefined || !Number.isFinite(db)) return null
  return Math.max(0, Math.min(100, ((db + 60) / 60) * 100))
}

const counters = computed(() => {
  const value = mic.value?.counters
  if (!value) return []
  return [
    { label: t('counterWakes'), value: value.wakes },
    { label: t('counterSegments'), value: value.segments },
    { label: t('counterFollowups'), value: value.followup_turns },
    { label: t('counterIgnored'), value: value.ignored_unwoken },
    { label: t('counterAsrFailed'), value: value.asr_failed },
    { label: t('counterDiscarded'), value: value.segments_discarded },
    { label: t('counterSelfEcho'), value: value.self_echo_suppressed },
  ]
})

const latency = computed(() => props.device?.latency ?? null)
</script>

<template>
  <Panel
    icon="mic" :title="t('micTitle')" :hint="t('micHint')">
    <template v-if="idleGated" #actions>
      <Pill tone="ok" size="sm">{{ t('micIdleGated') }}</Pill>
    </template>
    <EmptyState v-if="!mic" :title="t('noMic')" />

    <template v-else>
      <div class="mic__grid">
        <StatTile
          :label="t('micSession')"
          :value="idleGated ? t('micIdleGated') : sessionLabel"
          :tone="idleGated || mic.session === true ? 'ok' : mic.session === false ? 'warn' : 'muted'"
          :note="idleGated ? t('micIdleGatedNote') : mic.detail || ''"
        />
        <StatTile
          :label="t('micSpeech')"
          :value="mic.in_speech ? t('micInSpeech') : t('micIdle')"
          :tone="mic.in_speech ? 'live' : 'muted'"
        />
        <StatTile
          :label="t('micRate')"
          :value="(mic.chunks_per_sec ?? 0).toFixed(1)"
          unit="/s"
          :tone="idleGated ? 'ok' : 'muted'"
          :note="idleGated ? t('micIdleGated') : ''"
        />
        <StatTile :label="t('blocksSeen')" :value="mic.blocks_seen ?? 0" tone="muted" />
      </div>

      <div v-if="mic.levels" class="mic__levels">
        <Meter
          :label="t('micLevels')"
          :text="`${formatDb(mic.levels.p95_db)} / ${formatDb(mic.levels.max_db)}`"
          :value="levelPct(mic.levels.p95_db)"
          tone="ok"
        />
        <span class="mic__levels-detail mono">
          min {{ formatDb(mic.levels.min_db) }} · mean {{ formatDb(mic.levels.mean_db) }} · p95
          {{ formatDb(mic.levels.p95_db) }} · max {{ formatDb(mic.levels.max_db) }}
        </span>
      </div>

      <div class="mic__counters">
        <p class="mic__subtitle">{{ t('micCounters') }}</p>
        <div class="mic__counter-grid">
          <StatTile v-for="item in counters" :key="item.label" :label="item.label" :value="item.value" tone="muted" />
        </div>
      </div>

      <div class="mic__latency">
        <p class="mic__subtitle">{{ t('latencyTitle') }}</p>
        <div v-if="latency" class="mic__grid">
          <StatTile :label="t('latencySynth')" :value="formatDuration(latency.synth_mean_ms)" tone="accent" />
          <StatTile :label="t('latencyConfirm')" :value="formatDuration(latency.confirm_mean_ms)" tone="info" />
          <StatTile :label="t('latencySamples')" :value="latency.samples" tone="muted" />
        </div>
        <p v-else class="mic__none">{{ t('empty') }}</p>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.mic__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  gap: var(--sp-2);
}

.mic__levels {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.mic__levels-detail {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.mic__counters,
.mic__latency {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.mic__subtitle {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}

.mic__counter-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(7.5rem, 1fr));
  gap: var(--sp-2);
}

.mic__none {
  font-size: var(--fs-sm);
  color: var(--muted);
}
</style>
