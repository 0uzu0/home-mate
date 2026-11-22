<script setup lang="ts">
/**
 * The device list (RUI v2's list-then-detail idea): every reported device,
 * grouped by its household room, with its online state and identity. Selecting
 * a row sets the app-wide device that Status/Interaction/Devices follow.
 *
 * Rooms are labels on device metadata (no separate room object yet); an
 * unassigned device falls into a final group. With one device and no room the
 * group heading is suppressed so a single-Dot house stays quiet.
 */
import { computed } from 'vue'
import type { DeviceMeta, GatewayDevice, ModeDevice } from '../../api'
import { t } from '../../i18n'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import LedDot from '../LedDot.vue'
import AppIcon from '../AppIcon.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  devices: ModeDevice[]
  gatewayDevices: GatewayDevice[]
  meta: Record<string, DeviceMeta>
  selectedId: string | null
}>()

const emit = defineEmits<{ select: [id: string] }>()

const groups = computed<Array<[string, ModeDevice[]]>>(() => {
  const byRoom = new Map<string, ModeDevice[]>()
  for (const device of props.devices) {
    const room = props.meta[device.device]?.room?.trim() || ''
    const list = byRoom.get(room) ?? []
    list.push(device)
    byRoom.set(room, list)
  }
  return [...byRoom.entries()].sort((a, b) => {
    if (!a[0]) return 1
    if (!b[0]) return -1
    return a[0].localeCompare(b[0])
  })
})

function name(id: string): string {
  return props.meta[id]?.name?.trim() || id
}

function room(id: string): string {
  return props.meta[id]?.room?.trim() || ''
}

function gatewayFor(id: string): GatewayDevice | null {
  return props.gatewayDevices.find((device) => device.stable_id === id) ?? null
}
</script>

<template>
  <Panel icon="devices" :title="t('deviceListTitle')" :hint="t('deviceListHint')">
    <EmptyState v-if="!devices.length" :title="t('noDevice')" />
    <div v-else class="devlist">
      <div v-for="([roomName, list], groupIndex) in groups" :key="groupIndex">
        <p v-if="roomName || devices.length > 1" class="devlist__room">
          <AppIcon name="home" :size="13" />
          {{ roomName || t('roomUnassigned') }}
        </p>
        <button
          v-for="device in list"
          :key="device.device"
          type="button"
          class="devlist__row"
          :class="{ 'devlist__row--active': device.device === selectedId }"
          :aria-pressed="device.device === selectedId"
          @click="emit('select', device.device)"
        >
          <LedDot :state="device.assistant_state" size="md" :title="device.assistant_state || 'idle'" />
          <span class="devlist__identity">
            <span class="devlist__name">{{ name(device.device) }}</span>
            <span class="devlist__id mono">{{ device.device }}</span>
          </span>
          <span class="devlist__meta">
            {{ gatewayFor(device.device)?.model || t('unknown') }}
            <template v-if="gatewayFor(device.device)?.firmware">
              · {{ t('firmware') }}: {{ gatewayFor(device.device)?.firmware }}
            </template>
          </span>
          <Pill :tone="gatewayFor(device.device)?.online ? 'ok' : 'warn'" size="sm">
            {{ gatewayFor(device.device)?.online ? t('online') : t('offline') }}
          </Pill>
          <Pill size="sm" mono>
            {{ t('deviceCapabilities') }} {{ gatewayFor(device.device)?.capabilities.length ?? 0 }}
          </Pill>
          <span class="devlist__state" :class="{ 'devlist__state--on': device.device === selectedId }">
            <AppIcon v-if="device.device === selectedId" name="check" :size="14" />
            <span>{{ device.device === selectedId ? t('deviceSelected') : t('deviceSelect') }}</span>
          </span>
        </button>
      </div>
    </div>
  </Panel>
</template>

<style scoped>
.devlist {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.devlist__room {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  margin: var(--sp-1) 0 var(--sp-1);
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}

.devlist__row {
  display: grid;
  grid-template-columns: auto minmax(9rem, 0.9fr) minmax(12rem, 1.5fr) auto auto auto;
  align-items: center;
  gap: var(--sp-2);
  width: 100%;
  text-align: left;
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  color: var(--ink);
  padding: var(--sp-2) var(--sp-3);
  cursor: pointer;
  margin-bottom: var(--sp-1);
}

.devlist__row:hover {
  background: var(--surface-3);
}

.devlist__row--active {
  border-color: var(--accent-line);
  background: var(--accent-soft);
}

.devlist__identity {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.devlist__name {
  font-weight: 650;
}

.devlist__id {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.devlist__meta {
  font-size: var(--fs-xs);
  color: var(--muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.devlist__state {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  font-size: var(--fs-xs);
  color: var(--muted);
}

.devlist__state--on {
  color: var(--accent);
}

@media (max-width: 760px) {
  .devlist__row {
    grid-template-columns: auto 1fr auto;
  }

  .devlist__meta,
  .devlist__state {
    display: none;
  }
}
</style>
