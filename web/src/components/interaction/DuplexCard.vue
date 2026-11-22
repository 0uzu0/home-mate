<script setup lang="ts">
/**
 * Duplex and participation, per device. Capability / requested / negotiated /
 * effective are four separate facts: the page shows the effective value the
 * interaction matrix will read and never derives it.
 */
import { ref } from 'vue'
import { putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import StatTile from '../StatTile.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  refresh: () => void | Promise<void>
}>()

const { busy, message, tone, run } = useSave()
const reason = ref('')

async function request(device: ModeDevice, value: 'half' | 'full'): Promise<void> {
  reason.value = ''
  await run(async () => {
    const result = await putMode(device.device, { duplex_requested: value })
    reason.value = result.reason ?? ''
    await props.refresh()
  })
}
</script>

<template>
  <Panel
    icon="duplex" apply="immediate" :title="t('duplexTitle')" :hint="t('duplexHint')">
    <EmptyState v-if="!mode?.devices.length" :title="t('noDevice')" />

    <div v-for="device in mode?.devices ?? []" :key="device.device" class="duplex">
      <div class="duplex__head">
        <span class="duplex__name mono">{{ device.device }}</span>
        <Pill :tone="device.duplex.effective === 'full' ? 'ok' : 'info'" size="sm" mono>
          {{ t('duplexEffective') }}: {{ device.duplex.effective }}
        </Pill>
      </div>

      <div class="duplex__grid">
        <StatTile :label="t('duplexCapability')" :value="device.duplex.capability" tone="muted" />
        <StatTile :label="t('duplexRequested')" :value="device.duplex.requested" tone="accent" />
        <StatTile :label="t('duplexNegotiated')" :value="device.duplex.negotiated" tone="info" />
        <StatTile :label="t('duplexEffective')" :value="device.duplex.effective" tone="ok" />
      </div>

      <div class="duplex__row">
        <button
          type="button"
          class="btn"
          :disabled="busy || device.duplex.requested === 'full'"
          @click="request(device, 'full')"
        >
          {{ t('requestFull') }}
        </button>
        <button
          type="button"
          class="btn"
          :disabled="busy || device.duplex.requested === 'half'"
          @click="request(device, 'half')"
        >
          {{ t('requestHalf') }}
        </button>
        <span v-if="reason" class="duplex__reason">{{ t('duplexReason') }}: {{ reason }}</span>
      </div>
    </div>

    <p v-if="message" class="duplex__msg" :class="`duplex__msg--${tone}`">{{ message }}</p>
  </Panel>
</template>

<style scoped>
.duplex {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.duplex__head {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.duplex__name {
  font-size: var(--fs-base);
  font-weight: 650;
}

.duplex__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(8rem, 1fr));
  gap: var(--sp-2);
}

.duplex__row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.duplex__reason {
  font-size: var(--fs-xs);
  color: var(--warn);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.22rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.duplex__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.duplex__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
