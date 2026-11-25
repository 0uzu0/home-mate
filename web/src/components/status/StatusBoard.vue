<script setup lang="ts">
/**
 * Read-only status board: the four objects the light reflects (device,
 * session, program, outputs) plus the gateway link. Every value here is the
 * backend's; the page never derives a state from a guess.
 */
import { computed } from 'vue'
import { isDeviceIdleGated, type GatewayDevice, type GatewaySnapshot, type ModeDevice } from '../../api'
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

const matchedGatewayDevice = computed<GatewayDevice | null>(() => {
  if (props.gatewayDevice && (!props.device || props.gatewayDevice.stable_id === props.device.device)) {
    return props.gatewayDevice
  }
  const id = props.device?.device
  if (!id || !props.gateway?.devices?.length) return null
  return props.gateway.devices.find((d) => d.stable_id === id) ?? null
})

const idleGated = computed(() => isDeviceIdleGated(props.device, matchedGatewayDevice.value))

const muteLabel = computed(() => {
  if (props.device?.muted === true) return t('mutedOn')
  if (idleGated.value) return t('micIdleGated')
  if (props.device?.muted === false) return t('mutedOff')
  return t('mutedUnknown')
})

const muteSourceNote = computed(() => {
  if (props.device?.muted !== true && idleGated.value) return t('micIdleGatedNote')
  const src = props.device?.mute_source
  if (src === 'core_set') return t('muteSourceCore')
  if (src === 'button' || src === 'hardware') return t('muteSourceKey')
  if (src === 'device_boot') return t('muteSourceBoot')
  return t('deviceSovereign')
})

const deviceConnection = computed(() => {
  const id = props.device?.device
  if (!id || !props.gateway?.connections?.length) return null
  return props.gateway.connections.find((c) => c.devices.includes(id)) ?? null
})

const isLegacySingleGateway = computed(() => {
  if (!props.gateway) return false
  if (props.gateway.connections && props.gateway.connections.length > 0) return false
  return props.gateway.devices.length <= 1
})

const deviceGatewayConnected = computed(() => {
  if (!props.device) return false
  if (deviceConnection.value) {
    return matchedGatewayDevice.value ? matchedGatewayDevice.value.online : true
  }
  if (props.gateway?.connections?.length) {
    return false
  }
  if (matchedGatewayDevice.value) return matchedGatewayDevice.value.online
  return isLegacySingleGateway.value && Boolean(props.gateway?.connected)
})

const gatewayNote = computed(() => {
  if (!props.gateway) return ''
  const conn = deviceConnection.value
  if (conn) {
    const gen = `${t('generationLabel')} ${conn.generation} · ${conn.peer_addr || props.gateway.bind_addr}`
    const version = conn.gateway_version ? ` · ${t('gatewayVersionLabel')} ${conn.gateway_version}` : ''
    const proto = conn.protocol_operating != null ? ` · ${t('protocolLabel')} v${conn.protocol_operating}` : ''
    return `${gen}${version}${proto}`
  }
  if (isLegacySingleGateway.value) {
    const gen = `${t('generationLabel')} ${props.gateway.generation} · ${props.gateway.bind_addr}`
    const ver = props.gateway.gateway_version
    const version = ver ? ` · ${t('gatewayVersionLabel')} ${ver}` : ''
    const op = props.gateway.protocol_operating
    const proto = op != null ? ` · ${t('protocolLabel')} v${op}` : ''
    return `${gen}${version}${proto}`
  }
  if (matchedGatewayDevice.value) {
    return `${t('generationLabel')} ${matchedGatewayDevice.value.generation}`
  }
  return ''
})

const showLinkError = computed(() =>
  Boolean(props.gateway?.last_link_error) && !deviceGatewayConnected.value && isLegacySingleGateway.value,
)

const deviceOutputsCount = computed(() => props.device?.outputs?.length ?? 0)

const kwsStateLabel = computed(() => {
  if (idleGated.value) return t('micIdleGated')
  switch (props.device?.kws?.state) {
    case 'running':
      return t('kwsRunning')
    case 'starting':
      return t('kwsStarting')
    case 'off':
      return t('kwsOff')
    case 'down':
      return t('kwsDown')
    default:
      return t('kwsUnknown')
  }
})

const kwsStateTone = computed<'ok' | 'warn' | 'danger' | 'muted'>(() => {
  if (idleGated.value || props.device?.kws?.state === 'running') return 'ok'
  if (props.device?.kws?.state === 'starting') return 'warn'
  if (props.device?.kws?.state === 'down') return 'danger'
  return 'muted'
})

const muteTone = computed(() => {
  if (props.device?.muted === true) return 'danger' as const
  if (idleGated.value || props.device?.muted === false) return 'ok' as const
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
            {{ matchedGatewayDevice?.model || t('unknown') }}
            <template v-if="matchedGatewayDevice?.firmware"> · {{ t('firmware') }}: {{ matchedGatewayDevice.firmware }}</template>
            <template v-if="matchedGatewayDevice"> · {{ matchedGatewayDevice.online ? t('online') : t('offline') }}</template>
          </p>
        </div>
        <Pill :tone="matchedGatewayDevice?.online ? 'ok' : 'warn'" size="sm">
          {{ matchedGatewayDevice?.online ? t('online') : t('offline') }}
        </Pill>
        <Pill v-if="idleGated" tone="ok" size="sm">
          {{ t('micIdleGated') }}
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
        <StatTile
          :label="t('deviceKwsState')"
          :value="kwsStateLabel"
          :tone="kwsStateTone"
          :note="device.kws?.detail || ''"
        />
        <StatTile :label="t('duplexEffective')" :value="duplexLabel" tone="accent" :note="device.duplex.negotiated" />
        <!-- '参与' duplicated the conversation-state tile in different words;
             one tile (with the raw session state) is enough (audit 2026-11-14). -->
        <StatTile
          :label="t('gatewayLabel')"
          :value="deviceGatewayConnected ? t('connected') : t('disconnected')"
          :tone="deviceGatewayConnected ? 'ok' : 'danger'"
          :note="gatewayNote"
        />
        <!-- protocol v1 (#8/#11/#12): shown only when scoped to this single
             unconnected gateway; multi-gateway panels never mix sibling link errors. -->
        <StatTile
          v-if="showLinkError && gateway?.last_link_error"
          :label="t('linkErrorLabel')"
          :value="String(gateway.last_link_error.reason || gateway.last_link_error.kind)"
          tone="danger"
          :note="t('linkErrorNote')"
        />
        <StatTile
          :label="t('deviceOutputsCount')"
          :value="deviceOutputsCount"
          :tone="deviceOutputsCount > 0 ? 'warn' : 'muted'"
        />
        <StatTile
          :label="t('lastSeq')"
          :value="matchedGatewayDevice?.last_seq ?? '—'"
          tone="muted"
          :note="matchedGatewayDevice?.audio_profile || ''"
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
