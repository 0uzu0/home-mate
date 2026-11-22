<script setup lang="ts">
/**
 * Volume and microphone.
 *
 * Volume is two-way: the slider follows the DEVICE's last reported level
 * (boot report and the firmware's own volume keys), and Apply sends a new
 * level whose receipt moves the slider again. A local edit wins until it is
 * applied or explicitly re-read, so the 2 s poll cannot drag the control out
 * of the user's hand.
 */
import { computed, ref, watch } from 'vue'
import { setDeviceVolume, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import AppIcon from '../AppIcon.vue'
import Slider from '../Slider.vue'
import LedDot from '../LedDot.vue'

const props = defineProps<{
  device: ModeDevice | null
  gatewayDeviceId: string | null
  /** Last level the device reported (linear 0..1); null = not reported yet. */
  volume: number | null
}>()

const local = ref<number | null>(null)
const awaiting = ref(false)
const { busy, message, tone, run } = useSave()

const display = computed(() => local.value ?? props.volume ?? 0)
const reported = computed(() => props.volume != null)
/** The RECEIPT line is the device's number only: a dragged-but-unsaved draft
 * must never be presented as what the device reported (audit 2026-11-14). */
const reportedLevel = computed(() => Math.round((props.volume ?? 0) * 127))
const reportedPercent = computed(() => Math.round((props.volume ?? 0) * 100))
const level = computed(() => Math.round(display.value * 127))
const percent = computed(() => Math.round(display.value * 100))

const muted = computed(() => props.device?.muted)
const muteLabel = computed(() => {
  if (muted.value === true) return t('muteOn')
  if (muted.value === false) return t('muteOff')
  return t('muteUnknown')
})

const RADIUS = 42
const CIRCUMFERENCE = 2 * Math.PI * RADIUS
const dash = computed(() => {
  const filled = Math.max(0, Math.min(1, display.value)) * CIRCUMFERENCE
  return `${filled} ${CIRCUMFERENCE - filled}`
})

// A receipt equal to (or different from) the submitted value ends the local
// draft: the device is now the source of truth again.
watch(
  () => props.volume,
  (value) => {
    if (value != null && awaiting.value) {
      local.value = null
      awaiting.value = false
      message.value = t('volumeConfirmed', { level: Math.round(value * 127) })
      tone.value = 'ok'
    }
  },
)

function onVolume(value: number): void {
  local.value = value
  awaiting.value = false
}

function readDevice(): void {
  local.value = null
  awaiting.value = false
  message.value = ''
  tone.value = ''
}

async function apply(): Promise<void> {
  if (!props.gatewayDeviceId || !reported.value) return
  const target = props.gatewayDeviceId
  const value = Math.round(display.value * 100) / 100
  await run(async () => {
    const result = await setDeviceVolume(target, value)
    const confirmed =
      props.volume != null && Math.abs(props.volume - result.value) <= 0.005
    if (confirmed) {
      // The poll can observe the receipt before this response lands; do not
      // leave a draft (and a "waiting" message) behind in that race.
      local.value = null
      awaiting.value = false
      return t('volumeConfirmed', { level: Math.round(result.value * 127) })
    }
    local.value = result.value
    awaiting.value = true
    return t('volumeSubmitted', { level: Math.round(result.value * 127) })
  })
}
</script>

<template>
  <Panel icon="volume" apply="immediate" :title="t('volumeTitle')" :hint="t('volumeHint')">
    <template #actions>
      <Pill :tone="muted === true ? 'danger' : muted === false ? 'ok' : 'neutral'" size="sm">
        <!-- An unreported mute state is NOT "listening": the green hardware
             colour is reserved for a device fact (audit 2026-11-14). -->
        <LedDot :state="muted === true ? 'muted' : muted === false ? 'listening' : 'idle'" size="sm" />
        {{ muteLabel }}
      </Pill>
    </template>

    <div class="volume">
      <div class="volume__arc" aria-hidden="true">
        <svg viewBox="0 0 120 120" class="volume__svg">
          <circle cx="60" cy="60" :r="RADIUS" class="volume__shell" />
          <circle cx="60" cy="60" :r="RADIUS" class="volume__track" />
          <circle
            cx="60"
            cy="60"
            :r="RADIUS"
            class="volume__fill"
            :stroke-dasharray="dash"
            transform="rotate(-90 60 60)"
          />
        </svg>
        <span class="volume__readout mono">{{ percent }}%</span>
      </div>

      <div class="volume__controls">
        <Slider
          :model-value="display"
          :label="t('volumeLevel')"
          :min="0"
          :max="1"
          :step="0.01"
          :format="(value) => `${Math.round(value * 100)}%`"
          :disabled="!reported"
          @update:model-value="onVolume"
        />
        <div class="volume__row">
          <button
            type="button"
            class="btn btn--primary"
            :disabled="busy || !gatewayDeviceId || !reported"
            @click="apply"
          >
            <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('volumeApply') }}
          </button>
          <button type="button" class="btn" :disabled="!reported" @click="readDevice">
            <AppIcon name="reload" :size="13" /> {{ t('volumeReload') }}
          </button>
          <span v-if="message" class="volume__msg" :class="`volume__msg--${tone}`">{{ message }}</span>
        </div>
        <p class="volume__reported mono">
          {{
            reported
              ? `${t('volumeReported')}: ${reportedLevel}/127 (${reportedPercent}%)`
              : t('volumeUnknown')
          }}
        </p>
      </div>
    </div>

    <div class="volume__mute">
      <p class="volume__mute-title">{{ t('muteTitle') }}</p>
      <p class="volume__mute-hint">{{ t('muteHint') }}</p>
      <Pill size="sm" mono>{{ t('deviceSovereign') }}</Pill>
    </div>
  </Panel>
</template>

<style scoped>
.volume {
  display: flex;
  align-items: center;
  gap: var(--sp-5);
  flex-wrap: wrap;
}

.volume__arc {
  position: relative;
  width: 8rem;
  height: 8rem;
  flex: none;
}

.volume__svg {
  width: 100%;
  height: 100%;
}

.volume__shell {
  fill: var(--device-shell);
  stroke: var(--device-shell-2);
  stroke-width: 1;
}

.volume__track {
  fill: none;
  stroke: var(--device-arc-track);
  stroke-width: 7;
}

.volume__fill {
  fill: none;
  stroke: var(--device-arc);
  stroke-width: 7;
  stroke-linecap: round;
  transition: stroke-dasharray var(--dur-1) var(--ease);
}

.volume__readout {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: var(--fs-lg);
  color: var(--ink);
}

.volume__controls {
  flex: 1 1 16rem;
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  min-width: 0;
}

.volume__row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.volume__reported {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.volume__mute {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.volume__mute-title {
  font-size: var(--fs-sm);
  font-weight: 650;
}

.volume__mute-hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.btn {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
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

.volume__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.volume__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
