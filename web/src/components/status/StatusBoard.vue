<script setup lang="ts">
/**
 * Read-only status board: the four objects the light reflects (device,
 * session, program, outputs) plus the gateway link. Every value here is the
 * backend's; the page never derives a state from a guess.
 */
import { computed } from 'vue'
import type { GatewayDevice, GatewaySnapshot, ModeDevice } from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import Panel from '../Panel.vue'
import StatTile from '../StatTile.vue'
import Pill from '../Pill.vue'
import LedDot from '../LedDot.vue'
import ProvenanceTag from '../ProvenanceTag.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  device: ModeDevice | null
  gateway: GatewaySnapshot | null
  gatewayDevice: GatewayDevice | null
}>()

const activityTone = computed(() => {
  const activity = props.device?.activity ?? ''
  if (activity === 'idle') return 'muted' as const
  if (activity === 'speaking' || activity === 'music') return 'live' as const
  return 'accent' as const
})

const sessionLabel = computed(() => {
  switch (props.device?.session?.state) {
    case 'listening':
      return t('sessionOpen')
    case 'closing':
      return t('sessionClosing')
    case 'idle':
      return t('sessionClosed')
    default:
      return t('sessionUnknown')
  }
})

const sessionTone = computed(() => {
  switch (props.device?.session?.state) {
    case 'listening':
      return 'ok' as const
    case 'closing':
      return 'warn' as const
    default:
      return 'muted' as const
  }
})

const muteLabel = computed(() => {
  if (props.device?.muted === true) return t('mutedOn')
  if (props.device?.muted === false) return t('mutedOff')
  return t('mutedUnknown')
})

const muteSourceNote = computed(() => {
  const src = props.device?.mute_source
  if (src === 'core_set') return t('muteSourceCore')
  if (src === 'button' || src === 'hardware') return t('muteSourceKey')
  if (src === 'device_boot') return t('muteSourceBoot')
  return t('deviceSovereign')
})

const gatewayNote = computed(() => {
  if (!props.gateway) return ''
  const gen = `${t('generationLabel')} ${props.gateway.generation} · ${props.gateway.bind_addr}`
  const version = props.gateway.gateway_version
    ? ` · ${t('gatewayVersionLabel')} ${props.gateway.gateway_version}`
    : ''
  const proto =
    props.gateway.protocol_operating != null
      ? ` · ${t('protocolLabel')} v${props.gateway.protocol_operating}`
      : ''
  return `${gen}${version}${proto}`
})

const muteTone = computed(() => {
  if (props.device?.muted === true) return 'danger' as const
  if (props.device?.muted === false) return 'ok' as const
  return 'muted' as const
})

const duplexLabel = computed(() => {
  const value = props.device?.duplex.effective
  if (value === 'half') return t('duplexHalf')
  if (value === 'full') return t('duplexFull')
  return value || t('unknown')
})
</script>

<template>
  <Panel
    icon="gauge" :title="t('boardTitle')" :hint="t('boardHint')">
    <template #actions>
      <ProvenanceTag source="GET /api/v0/mode" :label="t('provenanceServer')" />
    </template>

    <EmptyState v-if="!device" :title="t('statusNoDevices')" />

    <template v-else>
      <div class="board__identity">
        <LedDot :state="device.assistant_state" size="lg" :title="device.assistant_state || 'idle'" />
        <div class="board__name">
          <p class="board__device mono">{{ device.device }}</p>
          <p class="board__meta">
            {{ gatewayDevice?.model || t('unknown') }}
            <template v-if="gatewayDevice?.firmware"> · {{ t('firmware') }}: {{ gatewayDevice.firmware }}</template>
            <template v-if="gatewayDevice"> · {{ gatewayDevice.online ? t('online') : t('offline') }}</template>
          </p>
        </div>
        <Pill :tone="gatewayDevice?.online ? 'ok' : 'warn'" size="sm">
          {{ gatewayDevice?.online ? t('online') : t('offline') }}
        </Pill>
        <Pill :tone="device.assistant_state ? 'accent' : 'neutral'" size="sm" mono>
          {{ t('assistantState') }}: {{ device.assistant_state || 'idle' }}
        </Pill>
      </div>

      <div class="board__grid">
        <StatTile
          :label="t('activity')"
          :value="device.activity"
          :tone="activityTone"
          :hint="t('boardHint')"
        />
        <StatTile
          :label="t('session')"
          :value="sessionLabel"
          :tone="sessionTone"
          :note="device.session?.since_ms ? formatTime(device.session.since_ms) : ''"
        />
        <StatTile
          :label="t('muted')"
          :value="muteLabel"
          :tone="muteTone"
          :note="muteSourceNote"
        />
        <StatTile :label="t('duplexEffective')" :value="duplexLabel" tone="accent" :note="device.duplex.negotiated" />
        <!-- '参与' duplicated the conversation-state tile in different words;
             one tile (with the raw session state) is enough (audit 2026-11-14). -->
        <StatTile
          :label="t('gatewayLabel')"
          :value="gateway?.connected ? t('connected') : t('disconnected')"
          :tone="gateway?.connected ? 'ok' : 'danger'"
          :note="gatewayNote"
        />
        <!-- protocol v1 (#8/#11/#12): the last link-level error the gateway
             REPORTED. Null = healthy; the tile disappears entirely instead of
             showing a fake 'ok'. -->
        <StatTile
          v-if="gateway?.last_link_error"
          :label="t('linkErrorLabel')"
          :value="String(gateway.last_link_error.reason || gateway.last_link_error.kind)"
          tone="danger"
          :note="t('linkErrorNote')"
        />
        <StatTile
          :label="t('pendingPlays')"
          :value="gateway?.pending_plays ?? 0"
          :tone="(gateway?.pending_plays ?? 0) > 0 ? 'warn' : 'muted'"
        />
        <StatTile
          :label="t('lastSeq')"
          :value="gatewayDevice?.last_seq ?? '—'"
          tone="muted"
          :note="gatewayDevice?.audio_profile || ''"
        />
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.board__identity {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.board__name {
  min-width: 0;
  flex: 1 1 auto;
}

.board__device {
  font-size: var(--fs-lg);
  font-weight: 650;
}

.board__meta {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.board__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9.5rem, 1fr));
  gap: var(--sp-2);
}
</style>
