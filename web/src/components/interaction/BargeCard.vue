<script setup lang="ts">
/**
 * Semantic barge-in: near-end speech during our own answer. The switch and
 * the answer-protection window are one PUT; the counters are the device's
 * truthful record of what happened.
 */
import { ref, watch } from 'vue'
import { putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import Slider from '../Slider.vue'
import StatTile from '../StatTile.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const enabled = ref<boolean | null>(null)
const protectS = ref<number | null>(null)
const { busy, message, tone, run } = useSave()

watch(
  () => props.mode,
  (mode) => {
    if (!mode) return
    if (enabled.value === null) enabled.value = mode.barge_in !== false
    if (protectS.value === null && mode.barge_protect_ms !== undefined) {
      protectS.value = Math.round(mode.barge_protect_ms / 100) / 10
    }
  },
  { immediate: true },
)

async function save(): Promise<void> {
  if (!props.device || enabled.value === null || protectS.value === null) return
  await run(async () => {
    await putMode(props.device!.device, {
      barge_in: enabled.value!,
      barge_protect_ms: Math.round(protectS.value! * 1000),
    })
    await props.refresh()
  })
}
</script>

<template>
  <Panel
    icon="barge" apply="immediate" :title="t('bargeTitle')" :hint="t('bargeHint')">
    <template #actions>
      <Pill v-if="device?.barge" size="sm" :tone="device.barge.protected ? 'warn' : 'neutral'">
        {{ t('bargeProtect') }}: {{ ((device.barge.protect_ms ?? 0) / 1000).toFixed(1) }} s
        <template v-if="device.barge.protected"> · {{ t('bargeProtected') }}</template>
      </Pill>
    </template>

    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <Toggle v-if="enabled !== null" v-model="enabled" :label="t('bargeEnabled')" />

      <ul class="barge__facts">
        <li>{{ t('bargeFactVoiceOnly') }}</li>
        <li>{{ t('bargeFactNextTurn') }}</li>
      </ul>

      <Slider
        v-if="protectS !== null"
        v-model="protectS"
        :label="t('bargeProtect')"
        :min="0"
        :max="3"
        :step="0.1"
        :format="(value) => `${value.toFixed(1)} s`"
        :hint="t('bargeProtectHint')"
      />

      <div class="barge__actions">
        <button type="button" class="btn btn--primary" :disabled="busy || enabled === null" @click="save">
          {{ busy ? t('saving') : t('bargeSave') }}
        </button>
        <span v-if="message" class="barge__msg" :class="`barge__msg--${tone}`">{{ message }}</span>
      </div>

      <div v-if="device.barge" class="barge__grid">
        <StatTile :label="t('bargeStops')" :value="device.barge.stops" tone="accent" />
        <StatTile :label="t('bargeIgnored')" :value="device.barge.ignored" tone="warn" />
        <StatTile
          :label="t('bargeLast')"
          :value="device.barge.last_barge_in_ms ? formatTime(device.barge.last_barge_in_ms) : '—'"
          tone="muted"
        />
        <StatTile
          :label="t('bargeGuarded')"
          :value="device.kws?.guarded ?? 0"
          tone="muted"
          :note="t('bargeGuarded')"
        />
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.barge__facts {
  margin: 0;
  padding-left: var(--sp-4);
  color: var(--muted);
  font-size: var(--fs-sm);
}

.barge__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  gap: var(--sp-2);
}

.barge__actions {
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
  padding: 0.24rem var(--sp-3);
  font-size: var(--fs-sm);
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

.barge__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.barge__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
