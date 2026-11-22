<script setup lang="ts">
/**
 * The light and the device facts it reflects, plus per-unit Night LED &
 * Orientation configuration wired to GET/PUT /api/v0/devices/:id/profile.
 */
import { computed, ref, watch } from 'vue'
import {
  LED_STATES,
  getDeviceProfile,
  ledStateOf,
  putDeviceProfile,
  type DeviceLedConfig,
  type GatewayDevice,
  type ModeDevice,
} from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { advancedMode } from '../../stores/advanced'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import StatTile from '../StatTile.vue'
import LedDot from '../LedDot.vue'
import ProvenanceTag from '../ProvenanceTag.vue'
import EmptyState from '../EmptyState.vue'
import Toggle from '../Toggle.vue'
import NumberField from '../NumberField.vue'

const props = defineProps<{
  device: ModeDevice | null
  gatewayDevice: GatewayDevice | null
}>()

const state = computed(() => props.device?.assistant_state || 'idle')
const led = computed(() => ledStateOf(state.value))

function ledStateLabel(s: string): string {
  switch (s) {
    case 'idle': return t('ledStateIdle')
    case 'listening': return t('ledStateListening')
    case 'wake': return t('ledStateWake')
    case 'thinking': return t('ledStateThinking')
    case 'speaking': return t('ledStateSpeaking')
    case 'music': return t('ledStateMusic')
    case 'paused': return t('ledStatePaused')
    case 'muted': return t('ledStateMuted')
    case 'error': return t('ledStateError')
    default: return s
  }
}

function minutesToTime(minutes: number): string {
  const clamped = Math.max(0, Math.min(1439, Math.round(minutes)))
  const hh = String(Math.floor(clamped / 60)).padStart(2, '0')
  const mm = String(clamped % 60).padStart(2, '0')
  return `${hh}:${mm}`
}

function timeToMinutes(value: string, fallback: number): number {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value.trim())
  if (!match) return fallback
  const hh = Number(match[1])
  const mm = Number(match[2])
  if (!Number.isFinite(hh) || !Number.isFinite(mm) || hh < 0 || hh > 23 || mm < 0 || mm > 59) {
    return fallback
  }
  return hh * 60 + mm
}

const nightStartTime = computed({
  get: () => minutesToTime(ledCfg.value.night_start_minute),
  set: (v: string) => {
    ledCfg.value.night_start_minute = timeToMinutes(v, ledCfg.value.night_start_minute)
  },
})

const nightEndTime = computed({
  get: () => minutesToTime(ledCfg.value.night_end_minute),
  set: (v: string) => {
    ledCfg.value.night_end_minute = timeToMinutes(v, ledCfg.value.night_end_minute)
  },
})

const ledCfg = ref<DeviceLedConfig>({
  night_enabled: false,
  night_start_minute: 1320,
  night_end_minute: 420,
  night_brightness_pct: 10,
  orientation_rotation: 0,
  orientation_reverse: false,
})

const { busy, message, tone, run } = useSave()

async function loadProfile(id: string): Promise<void> {
  try {
    const res = await getDeviceProfile(id)
    if (res.led) {
      ledCfg.value = { ...res.led }
    }
  } catch {
    /* keep defaults until reachable */
  }
}

watch(
  () => props.device?.device,
  (id) => {
    if (id) void loadProfile(id)
  },
  { immediate: true },
)

async function saveLedProfile(): Promise<void> {
  const id = props.device?.device
  if (!id) return
  await run(async () => {
    const res = await putDeviceProfile(id, { led: ledCfg.value })
    if (res.led) ledCfg.value = { ...res.led }
    return t('ledProfileSaved')
  })
}
</script>

<template>
  <Panel icon="led" :title="t('ledTitle')" :hint="t('ledHint')">
    <template #actions>
      <ProvenanceTag source="assistant_state" :label="t('provenanceServer')" />
    </template>

    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <div class="ledcard__now">
        <div class="ledcard__ring">
          <LedDot :state="state" size="xl" :title="state" />
        </div>
        <div class="ledcard__text">
          <p class="ledcard__state">{{ ledStateLabel(state) }}</p>
          <p class="ledcard__hint">
            {{ t('ledCurrent') }}: <span>{{ ledStateLabel(led) }}</span>
          </p>
        </div>
      </div>

      <div class="ledcard__legend">
        <div class="ledcard__legend-head">
          <span>{{ t('ledLegend') }}</span>
          <span class="ledcard__physical">{{ t('ledPhysicalHint') }}</span>
        </div>
        <ul class="ledcard__swatches">
          <li v-for="item in LED_STATES" :key="item" class="ledcard__swatch">
            <LedDot :state="item" size="sm" :title="ledStateLabel(item)" />
            <span>{{ ledStateLabel(item) }}</span>
          </li>
        </ul>
      </div>

      <div class="ledcard__profile">
        <p class="ledcard__profile-title">{{ t('ledNightTitle') }}</p>
        <div class="ledcard__toggles">
          <Toggle v-model="ledCfg.night_enabled" :label="t('ledNightEnabled')" />
          <Toggle v-model="ledCfg.orientation_reverse" :label="t('ledOrientationReverse')" />
        </div>
        <div class="ledcard__fields">
          <label class="ledcard__time-field">
            <span class="ledcard__time-label">{{ t('ledNightStart') }}</span>
            <input v-model="nightStartTime" type="time" class="ledcard__time-input mono" />
          </label>
          <label class="ledcard__time-field">
            <span class="ledcard__time-label">{{ t('ledNightEnd') }}</span>
            <input v-model="nightEndTime" type="time" class="ledcard__time-input mono" />
          </label>
          <NumberField
            v-model="ledCfg.night_brightness_pct"
            :label="t('ledNightBrightness')"
            :min="0"
            :max="100"
          />
          <NumberField
            v-model="ledCfg.orientation_rotation"
            :label="t('ledOrientationRotation')"
            :min="0"
            :max="11"
          />
        </div>
        <div class="ledcard__actions">
          <button type="button" class="btn btn--primary" :disabled="busy" @click="saveLedProfile">
            {{ busy ? t('saving') : t('ledProfileSave') }}
          </button>
          <span v-if="message" :class="`ledcard__msg--${tone}`">{{ message }}</span>
        </div>
      </div>

      <div class="ledcard__grid">
        <StatTile :label="t('deviceLabel')" :value="device.device" tone="muted" />
        <StatTile :label="t('firmware')" :value="gatewayDevice?.firmware ?? '—'" tone="muted" />
        <StatTile :label="t('model')" :value="gatewayDevice?.model ?? '—'" tone="muted" />
        <StatTile v-if="advancedMode" :label="t('lastSeq')" :value="gatewayDevice?.last_seq ?? '—'" tone="muted" />
      </div>

      <div v-if="advancedMode && gatewayDevice?.capabilities.length" class="ledcard__caps">
        <Pill v-for="capability in gatewayDevice.capabilities" :key="capability" size="sm" mono>
          {{ capability }}
        </Pill>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.ledcard__now {
  display: flex;
  align-items: center;
  gap: var(--sp-4);
}

.ledcard__ring {
  padding: var(--sp-3);
  border-radius: 50%;
  background: var(--device-shell);
  border: 1px solid var(--device-shell-2);
  display: inline-flex;
}

.ledcard__state {
  font-size: var(--fs-lg);
  font-weight: 650;
}

.ledcard__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.ledcard__legend,
.ledcard__profile {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.ledcard__profile-title {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--ink-2);
}

.ledcard__toggles {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-4);
}

.ledcard__fields {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(11rem, 1fr));
  gap: var(--sp-2);
}

.ledcard__time-field {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.ledcard__time-label {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.ledcard__time-input {
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.25rem var(--sp-2);
  font-size: var(--fs-sm);
}

.ledcard__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.ledcard__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.ledcard__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
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

.ledcard__legend-head {
  display: flex;
  justify-content: space-between;
  gap: var(--sp-3);
  flex-wrap: wrap;
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}

.ledcard__physical {
  text-transform: none;
  letter-spacing: normal;
}

.ledcard__swatches {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-3);
}

.ledcard__swatch {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  font-size: var(--fs-xs);
  color: var(--ink-2);
}

.ledcard__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  gap: var(--sp-2);
}

.ledcard__caps {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-1);
}
</style>
