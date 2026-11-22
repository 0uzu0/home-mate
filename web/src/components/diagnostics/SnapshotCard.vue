<script setup lang="ts">
/**
 * Passive snapshot: gateway, services, per-device activity, event counters,
 * recent errors and failed traces. No side effects; a missing fact says so.
 */
import { computed } from 'vue'
import type { Diagnostics, GatewayDevice, NorthboundStatus } from '../../api'
import { t } from '../../i18n'
import { errorDetail, formatDateTime, formatTime } from '../../format'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import StatTile from '../StatTile.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'
import EmptyState from '../EmptyState.vue'
import { formatEvent, kindLabel } from './eventText'

const props = defineProps<{
  data: Diagnostics | null
  error: unknown
  loading: boolean
  refresh: () => void | Promise<void>
}>()

const voiceCounts = computed(() => Object.entries(props.data?.voice_counts ?? {}).sort((a, b) => b[1] - a[1]))
const gatewayCounts = computed(() =>
  Object.entries(props.data?.gateway_counts ?? {}).sort((a, b) => b[1] - a[1]),
)

function transportSample(device: GatewayDevice): NorthboundStatus | null {
  const report = device.northbound
  const sample = report?.sample
  if (report?.state !== 'available' || report.schema !== 1 || sample?.schema !== 1) return null
  const counters = Object.entries(sample).filter(([key]) => !['boot_id', 'last_fault'].includes(key))
  if (typeof sample.boot_id !== 'string' || !sample.boot_id || counters.some(([, value]) => typeof value !== 'number' || !Number.isInteger(value) || value < 0)) return null
  const required = ['link_generation', 'sampled_uptime_ms', 'critical_current_count', 'critical_high_water_count',
    'critical_current_bytes', 'critical_high_water_bytes', 'health_coalesced', 'presence_coalesced', 'critical_full',
    'invalid_frames', 'audio_dropped', 'audio_current_count', 'audio_high_water_count', 'writer_timeouts', 'writer_errors',
    'transport_disconnects', 'oldest_pending_ms', 'oldest_pending_high_water_ms', 'successful_write_bytes'] as const
  const known = new Set<string>(['schema', 'boot_id', 'last_fault', ...required])
  if (Object.keys(sample).some(key => !known.has(key)) || !Object.hasOwn(sample, 'last_fault')) return null
  const fault = sample.last_fault
  const faultCodes = ['critical_count_full', 'critical_bytes_full', 'invalid_source', 'oversized_frame', 'encode_failed',
    'invalid_frame', 'writer_timeout', 'writer_error', 'writer_panic', 'reader_error', 'peer_closed', 'decode_error', 'protocol_mismatch']
  if (fault !== null && (!fault || !faultCodes.includes(fault.code) || !Number.isInteger(fault.uptime_ms) || fault.uptime_ms < 0)) return null
  return required.every(key => typeof sample[key] === 'number') ? sample : null
}
function transportState(device: GatewayDevice): string {
  const report = device.northbound
  if (!report || report.state === 'absent') return t('northboundAbsent')
  if (report.state === 'unsupported' || (report.schema != null && report.schema !== 1)) return t('northboundUnsupported')
  if (!transportSample(device)) return t('northboundInvalid')
  return props.error || report.stale || !props.data?.gateway.connected || !device.online
      || report.connection_generation !== device.generation || device.generation !== props.data.gateway.generation
    ? t('northboundStale') : t('northboundReported')
}
</script>

<template>
  <Panel
    icon="snapshot" :title="t('snapshotTitle')" :hint="t('snapshotHint')">
    <template #actions>
      <button type="button" class="btn" :disabled="loading" @click="refresh"><AppIcon name="reload" :size="13" /> {{ t('refresh') }}</button>
    </template>

    <LoadingBlock v-if="loading && !data" :label="t('loading')" />
    <ErrorNotice v-else-if="error && !data" :error="error" @retry="refresh" />

    <template v-else-if="data">
      <section class="snap">
        <p class="snap__title">{{ t('snapshotGateway') }}</p>
        <div class="snap__grid">
          <StatTile
            :label="t('gatewayLabel')"
            :value="data.gateway.connected ? t('connected') : t('disconnected')"
            :tone="data.gateway.connected ? 'ok' : 'danger'"
            :note="data.gateway.bind_addr"
          />
          <StatTile :label="t('generationLabel')" :value="data.gateway.generation" tone="muted" />
          <StatTile :label="t('pendingPlays')" :value="data.gateway.pending_plays" tone="muted" />
        </div>
        <div class="snap__pills">
          <template v-for="device in data.gateway.devices" :key="device.stable_id">
            <Pill :tone="device.online ? 'ok' : 'danger'" size="sm" mono>
              {{ device.stable_id }} · {{ device.online ? t('online') : t('offline') }}
              <template v-if="device.aec_tail_ms != null"> · {{ t('snapshotAecTail') }} {{ device.aec_tail_ms }}ms</template>
              <template v-if="device.echo_health"> · {{ t('snapshotEchoHealth') }} {{ device.echo_health }}</template>
            </Pill>
            <Pill v-if="device.tail_mismatch" tone="warn" size="sm" mono>
              {{ device.stable_id }} · {{ t('snapshotTailMismatch') }}
            </Pill>
          </template>
        </div>
        <div v-for="device in data.devices" :key="device.device" class="snap__device mono">
          {{ device.device }} · {{ t('activity') }}: {{ device.activity }} · {{ t('duplexEffective') }}:
          {{ device.duplex.effective }} · {{ t('outputStage') }}: {{ device.outputs.length }}
        </div>
        <div v-for="device in data.gateway.devices" :key="`north-${device.stable_id}`" class="snap__transport">
          <p class="snap__title">{{ t('northboundTitle') }} · {{ device.stable_id }}</p>
          <p class="snap__device" role="status">{{ transportState(device) }}</p>
          <p class="snap__device">{{ t('northboundLastReported') }}:
            {{ device.northbound?.received_ms != null ? formatDateTime(device.northbound.received_ms) : '—' }}</p>
          <template v-if="transportSample(device)">
            <p class="snap__device mono">{{ t('northboundBoot') }}: {{ transportSample(device)!.boot_id }} ·
              {{ t('northboundAttempt') }}: {{ transportSample(device)!.link_generation }}</p>
            <dl class="snap__transport-stats">
              <dt>{{ t('northboundCriticalCount') }}</dt><dd>{{ transportSample(device)!.critical_current_count }} / {{ transportSample(device)!.critical_high_water_count }}</dd>
              <dt>{{ t('northboundCriticalBytes') }}</dt><dd>{{ transportSample(device)!.critical_current_bytes }} / {{ transportSample(device)!.critical_high_water_bytes }}</dd>
              <dt>{{ t('northboundCoalesced') }}</dt><dd>{{ transportSample(device)!.health_coalesced }} / {{ transportSample(device)!.presence_coalesced }}</dd>
              <dt>{{ t('northboundAudio') }}</dt><dd>{{ transportSample(device)!.audio_dropped }} / {{ transportSample(device)!.audio_high_water_count }}</dd>
              <dt>{{ t('northboundWriterFaults') }}</dt><dd>{{ transportSample(device)!.writer_timeouts }} / {{ transportSample(device)!.writer_errors }}</dd>
              <dt>{{ t('northboundOldest') }}</dt><dd>{{ transportSample(device)!.oldest_pending_ms }} / {{ transportSample(device)!.oldest_pending_high_water_ms }} ms</dd>
              <dt>{{ t('northboundLastFault') }}</dt><dd>{{ transportSample(device)!.last_fault?.code ?? t('northboundNoFault') }}
                <template v-if="transportSample(device)!.last_fault"> · {{ transportSample(device)!.last_fault!.uptime_ms }} ms</template></dd>
            </dl>
          </template>
        </div>
      </section>

      <section class="snap">
        <p class="snap__title">{{ t('snapshotServices') }}</p>
        <div class="snap__pills">
          <Pill
            v-for="(service, name) in data.services"
            :key="name"
            :tone="service.configured ? 'ok' : 'warn'"
            size="sm"
            mono
          >
            {{ String(name).toUpperCase() }} {{ service.provider || '—' }}
          </Pill>
        </div>
      </section>

      <div class="snap__cols">
        <section class="snap">
          <p class="snap__title">{{ t('snapshotCounters') }}</p>
          <table class="snap__table">
            <tbody>
              <tr v-for="[kind, count] in voiceCounts" :key="`v-${kind}`">
                <td class="mono muted">{{ kindLabel(kind) }}</td>
                <td class="mono">{{ count }}</td>
              </tr>
              <tr v-for="[kind, count] in gatewayCounts" :key="`g-${kind}`">
                <td class="mono muted">{{ t('snapshotGatewayCounters') }} · {{ kind }}</td>
                <td class="mono">{{ count }}</td>
              </tr>
            </tbody>
          </table>
          <EmptyState v-if="!voiceCounts.length && !gatewayCounts.length" :title="t('empty')" />
        </section>

        <section class="snap">
          <p class="snap__title">{{ t('snapshotErrors') }}</p>
          <ul v-if="data.recent_errors.length" class="snap__list">
            <li v-for="(event, index) in data.recent_errors" :key="`e-${index}`">
              <span class="mono muted">{{ formatTime(event.ts_ms) }}</span>
              <span>{{ formatEvent(event) }}</span>
            </li>
          </ul>
          <EmptyState v-else :title="t('snapshotNoErrors')" />
        </section>

        <section class="snap">
          <p class="snap__title">{{ t('snapshotFailedTraces') }}</p>
          <ul v-if="data.failed_traces.length" class="snap__list">
            <li v-for="trace in data.failed_traces" :key="trace.id">
              <span class="mono muted">{{ formatDateTime(trace.created_ms) }}</span>
              <span>{{ trace.input_text }} · {{ trace.status }}</span>
            </li>
          </ul>
          <EmptyState v-else :title="t('snapshotNoFailed')" />
        </section>
      </div>

      <p v-if="error" class="snap__stale">{{ errorDetail(error) }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.snap {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  min-width: 0;
}

.snap__title {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}

.snap__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  gap: var(--sp-2);
}

.snap__pills {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-1);
}

.snap__device {
  font-size: var(--fs-xs);
  color: var(--ink-2);
}
.snap__transport { border-top: 1px solid var(--line); padding-top: var(--sp-2); min-width: 0; overflow-wrap: anywhere; }
.snap__transport-stats { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: var(--sp-1) var(--sp-3); font-size: var(--fs-xs); margin: 0; }
.snap__transport-stats dt { color: var(--muted); }
.snap__transport-stats dd { margin: 0; font-variant-numeric: tabular-nums; }

.snap__cols {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.snap__table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-xs);
}

.snap__table td {
  padding: 0.12rem var(--sp-2) 0.12rem 0;
}

.snap__list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  font-size: var(--fs-xs);
  max-height: 14rem;
  overflow-y: auto;
}

.snap__list li {
  display: flex;
  gap: var(--sp-2);
  align-items: baseline;
}

.muted {
  color: var(--muted);
}

.snap__stale {
  font-size: var(--fs-xs);
  color: var(--warn);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.2rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
