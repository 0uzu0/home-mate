<script setup lang="ts">
/**
 * Outputs: every play channel and its real stage, plus the one Stop control.
 * Stop submits cancellation; only a matching flush confirms hardware silence.
 */
import { computed } from 'vue'
import { stopDevice, type GatewaySnapshot, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import EmptyState from '../EmptyState.vue'
import Pill from '../Pill.vue'

const props = defineProps<{
  device: ModeDevice | null
  gateway: GatewaySnapshot | null
  refresh: () => void | Promise<void>
}>()

const { busy, message, tone, run } = useSave()
const outputs = computed(() => props.device?.outputs ?? [])
// A play that was asked to stop stays in the backend list at `stopping`.
// That is a fact, not an active output — show it, but explain it, so the
// card cannot read as "four things are playing" while activity is idle.
const hasStopping = computed(() => outputs.value.some((output) => output.stage === 'stopping'))

async function stop(): Promise<void> {
  if (!props.device) return
  await run(async () => {
    const result = await stopDevice(props.device!.device)
    await props.refresh()
    return result.note
  })
}
</script>

<template>
  <Panel
    icon="outputs" :title="t('outputsTitle')" :hint="t('outputsHint')">
    <template #actions>
      <button type="button" class="btn btn--danger" :disabled="busy || !device" @click="stop">
        <AppIcon name="stop" :size="13" /> {{ t('stopAll') }}
      </button>
    </template>

    <div v-if="gateway" class="outputs__meta">
      <Pill size="sm" mono>{{ t('pendingPlays') }}: {{ gateway.pending_plays }}</Pill>
      <Pill size="sm" mono>{{ t('gatewayLabel') }}: {{ gateway.connected ? t('connected') : t('disconnected') }}</Pill>
    </div>

    <EmptyState v-if="!outputs.length" :title="t('noOutputs')" />

    <div v-else class="outputs__table">
      <div class="outputs__head">
        <span>{{ t('outputChannel') }}</span>
        <span>{{ t('outputPlayId') }}</span>
        <span>{{ t('outputStage') }}</span>
        <span>{{ t('outputStarted') }}</span>
        <span>{{ t('outputTask') }}</span>
      </div>
      <div v-for="output in outputs" :key="output.play_id" class="outputs__row">
        <Pill size="sm" :tone="output.channel === 'media' ? 'info' : 'accent'" mono>
          {{ output.channel }}
        </Pill>
        <span class="mono outputs__id">{{ output.play_id }}</span>
        <Pill size="sm" :tone="output.stage === 'unknown' ? 'warn' : output.stage === 'stopping' ? 'muted' : 'neutral'" mono>
          {{ output.stage }}
        </Pill>
        <span class="mono">{{ output.started_ms ? formatTime(output.started_ms) : '—' }}</span>
        <span class="mono outputs__task">{{ output.task || '—' }}</span>
      </div>
    </div>

    <p v-if="hasStopping" class="outputs__stopping">{{ t('outputStoppingHint') }}</p>

    <p v-if="message" class="outputs__msg" :class="`outputs__msg--${tone}`">
      {{ t('stopResult') }}: {{ message }}
    </p>

    <p v-if="device?.current_task" class="outputs__current mono">
      {{ t('outputTask') }}: {{ device.current_task }}
    </p>
  </Panel>
</template>

<style scoped>
.outputs__meta {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.outputs__table {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  overflow-x: auto;
}

.outputs__head,
.outputs__row {
  display: grid;
  grid-template-columns: 6rem minmax(10rem, 1.5fr) 7rem 7rem minmax(8rem, 1fr);
  gap: var(--sp-2);
  align-items: center;
  min-width: 40rem;
}

.outputs__head {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
  padding-bottom: var(--sp-1);
  border-bottom: 1px solid var(--line);
}

.outputs__row {
  font-size: var(--fs-sm);
  padding: var(--sp-1) 0;
  border-bottom: 1px solid var(--line);
}

.outputs__id,
.outputs__task {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.btn--danger:hover:not(:disabled) {
  background: var(--danger);
  color: var(--surface);
}

.outputs__stopping {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.outputs__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.outputs__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.outputs__current {
  font-size: var(--fs-xs);
  color: var(--muted);
}
</style>
