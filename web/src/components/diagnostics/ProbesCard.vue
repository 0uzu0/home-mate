<script setup lang="ts">
/**
 * Active probes and repairs. Everything here has a real side effect (sound,
 * re-sent state), so each action is a separate button with its own honest
 * result — no probe is fired automatically on page load.
 */
import { computed, ref, watch } from 'vue'
import {
  isDeviceRequiredError,
  reapplyModeRepair,
  runMicArrayTest,
  runPlayAssetTest,
  runPlaybackLoopbackTest,
  runVoicePlayTest,
  type LoopbackResult,
  type MicArrayResult,
  type ModeDevice,
  type PlayAssetResult,
  type VoicePlayTestResult,
} from '../../api'
import { deviceName, selectedId as storeSelectedId } from '../../stores/devices'
import { t, type Key } from '../../i18n'
import { errorDetail, formatDuration } from '../../format'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import SectionLabel from '../SectionLabel.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  devices: ModeDevice[]
  selectedDeviceId?: string | null
}>()

type ProbeState = { ok: boolean; detail: string }

const preferredDeviceId = computed(() =>
  props.selectedDeviceId !== undefined ? props.selectedDeviceId ?? '' : storeSelectedId.value ?? '',
)

const selectedDevice = ref('')
watch(
  [() => props.devices, preferredDeviceId],
  ([list, pref], [, prevPref]) => {
    if (!list.length) {
      selectedDevice.value = ''
      return
    }
    const prefChanged = prevPref !== undefined && pref !== prevPref
    if (pref && list.some((d) => d.device === pref) && (prefChanged || !selectedDevice.value || !list.some((d) => d.device === selectedDevice.value))) {
      selectedDevice.value = pref
      return
    }
    if (selectedDevice.value && list.some((d) => d.device === selectedDevice.value)) {
      return
    }
    selectedDevice.value = list.length === 1 ? list[0].device : ''
  },
  { immediate: true },
)

const voice = ref<ProbeState | null>(null)
const asset = ref<ProbeState | null>(null)
const loopback = ref<ProbeState | null>(null)
const mics = ref<ProbeState | null>(null)
const repair = ref<ProbeState | null>(null)
const busy = ref('')

function probeErrorMessage(err: unknown): string {
  return isDeviceRequiredError(err) ? t('deviceRequiredError') : errorDetail(err)
}

function targetOrGuard(): string | null {
  const target = selectedDevice.value.trim()
  if (!target && props.devices.length > 1) return null
  return target
}

const LOOP_KEYS: Record<string, Key> = {
  loopback_intact: 'lbIntact',
  loopback_transformed: 'lbTransformed',
  weak_relationship: 'lbWeak',
  no_relationship: 'lbNone',
  capture_silent: 'lbCaptureSilent',
  write_silent: 'lbWriteSilent',
  insufficient_data: 'lbInsufficient',
}

function loopText(result: LoopbackResult): string {
  const key = result.loopback ? LOOP_KEYS[result.loopback.verdict] : undefined
  return key ? t(key) : result.loopback?.verdict || result.error || 'failed'
}

async function runVoice(): Promise<void> {
  const target = targetOrGuard()
  if (target === null) {
    voice.value = { ok: false, detail: t('deviceRequiredError') }
    return
  }
  busy.value = 'voice'
  voice.value = null
  try {
    const result: VoicePlayTestResult = await runVoicePlayTest(target || undefined)
    voice.value = result.ok
      ? {
          ok: true,
          detail: `${result.device ?? ''} · ${result.stage ?? ''} · ${formatDuration(result.synth_ms)} / ${formatDuration(result.total_ms)} · ${result.provider ?? ''} ${result.voice ?? ''}`,
        }
      : {
          ok: false,
          detail: isDeviceRequiredError(result)
            ? t('deviceRequiredError')
            : result.error || result.stage || 'failed',
        }
  } catch (err) {
    voice.value = { ok: false, detail: probeErrorMessage(err) }
  } finally {
    busy.value = ''
  }
}

async function runAsset(): Promise<void> {
  const target = targetOrGuard()
  if (target === null) {
    asset.value = { ok: false, detail: t('deviceRequiredError') }
    return
  }
  busy.value = 'asset'
  asset.value = null
  try {
    const result: PlayAssetResult = await runPlayAssetTest(
      'erle-stimulus.wav',
      undefined,
      target || undefined,
    )
    asset.value = result.ok
      ? {
          ok: true,
          detail: `${result.device ?? ''} · ${result.asset ?? ''} · ${result.stage ?? ''} · ${formatDuration(result.duration_ms)} / ${formatDuration(result.total_ms)}`,
        }
      : {
          ok: false,
          detail: isDeviceRequiredError(result)
            ? t('deviceRequiredError')
            : result.error || result.stage || 'failed',
        }
  } catch (err) {
    asset.value = { ok: false, detail: probeErrorMessage(err) }
  } finally {
    busy.value = ''
  }
}

async function runLoopback(): Promise<void> {
  const target = targetOrGuard()
  if (target === null) {
    loopback.value = { ok: false, detail: t('deviceRequiredError') }
    return
  }
  busy.value = 'loopback'
  loopback.value = null
  try {
    const result = await runPlaybackLoopbackTest(undefined, target || undefined)
    if (!result.ok && (isDeviceRequiredError(result) || isDeviceRequiredError(result.play))) {
      loopback.value = { ok: false, detail: t('deviceRequiredError') }
      return
    }
    const report = result.loopback
    loopback.value = {
      ok: result.ok,
      detail: `${loopText(result)}${
        report
          ? ` · corr ${report.correlation ?? '—'} · lag ${report.lag_samples ?? '—'} · zero ${report.longest_zero_ms}ms`
          : ''
      }`,
    }
  } catch (err) {
    loopback.value = { ok: false, detail: probeErrorMessage(err) }
  } finally {
    busy.value = ''
  }
}

async function runMics(): Promise<void> {
  const target = targetOrGuard()
  if (target === null) {
    mics.value = { ok: false, detail: t('deviceRequiredError') }
    return
  }
  busy.value = 'mics'
  mics.value = null
  try {
    const result: MicArrayResult = await runMicArrayTest(undefined, target || undefined)
    if (!result.ok && (isDeviceRequiredError(result) || isDeviceRequiredError(result.play))) {
      mics.value = { ok: false, detail: t('deviceRequiredError') }
      return
    }
    mics.value = result.ok
      ? {
          ok: true,
          detail: `${result.play?.device ?? target} · frames ${result.mics?.frames ?? 0} · active ${formatDuration(result.mics?.active_ms)}`,
        }
      : { ok: false, detail: result.error || result.play?.error || 'failed' }
  } catch (err) {
    mics.value = { ok: false, detail: probeErrorMessage(err) }
  } finally {
    busy.value = ''
  }
}

async function reapply(device: string): Promise<void> {
  busy.value = `repair-${device}`
  repair.value = null
  try {
    const result = await reapplyModeRepair(device)
    repair.value = {
      ok: true,
      detail: `${result.device}: ${result.state} (requested=${result.requested})`,
    }
  } catch (err) {
    repair.value = { ok: false, detail: errorDetail(err) }
  } finally {
    busy.value = ''
  }
}
</script>

<template>
  <Panel
    icon="probes" :title="t('probesTitle')" :hint="t('probesHint')">
    <template v-if="devices.length > 1" #actions>
      <select
        v-model="selectedDevice"
        class="btn mono"
        :aria-label="t('probeTargetLabel')"
      >
        <option value="" disabled>{{ t('selectTargetDevice') }}</option>
        <option v-for="d in devices" :key="d.device" :value="d.device">
          {{ deviceName(d.device) === d.device ? d.device : `${deviceName(d.device)} (${d.device})` }}
        </option>
      </select>
    </template>
    <template v-else-if="selectedDevice" #actions>
      <Pill size="sm" tone="accent" mono>{{ t('scopeThisDevice', { device: selectedDevice }) }}</Pill>
    </template>
    <section class="probe">
      <div class="probe__row">
        <button type="button" class="btn" :disabled="busy !== ''" @click="runVoice">
          {{ busy === 'voice' ? t('probeRunning') : t('voiceTest') }}
        </button>
        <Pill v-if="voice" :tone="voice.ok ? 'ok' : 'danger'" size="sm">
          {{ voice.ok ? '✓' : '✗' }} {{ voice.detail }}
        </Pill>
      </div>
      <p class="probe__hint">{{ t('voiceTestHint') }}</p>
    </section>

    <section class="probe">
      <div class="probe__row">
        <button type="button" class="btn" :disabled="busy !== ''" @click="runAsset">
          {{ busy === 'asset' ? t('probeRunning') : t('assetTest') }}
        </button>
        <Pill v-if="asset" :tone="asset.ok ? 'ok' : 'danger'" size="sm">
          {{ asset.ok ? '✓' : '✗' }} {{ asset.detail }}
        </Pill>
      </div>
      <p class="probe__hint">{{ t('assetTestHint') }}</p>
    </section>

    <section class="probe">
      <div class="probe__row">
        <button type="button" class="btn" :disabled="busy !== ''" @click="runLoopback">
          {{ busy === 'loopback' ? t('probeRunning') : t('loopbackTest') }}
        </button>
        <Pill v-if="loopback" :tone="loopback.ok ? 'ok' : 'danger'" size="sm">
          {{ loopback.ok ? '✓' : '✗' }} {{ loopback.detail }}
        </Pill>
      </div>
      <p class="probe__hint">{{ t('loopbackTestHint') }}</p>
    </section>

    <section class="probe">
      <div class="probe__row">
        <button type="button" class="btn" :disabled="busy !== ''" @click="runMics">
          {{ busy === 'mics' ? t('probeRunning') : t('micArrayTest') }}
        </button>
        <Pill v-if="mics" :tone="mics.ok ? 'ok' : 'danger'" size="sm">
          {{ mics.ok ? '✓' : '✗' }} {{ mics.detail }}
        </Pill>
      </div>
      <p class="probe__hint">{{ t('micArrayTestHint') }}</p>
    </section>

    <section class="probe">
      <SectionLabel :text="t('repairTitle')" />
      <EmptyState v-if="!devices.length" :title="t('noDevice')" />
      <div v-else class="probe__repairs">
        <div v-for="device in devices" :key="device.device" class="probe__row">
          <span class="mono">{{ device.device }}</span>
          <button type="button" class="btn" :disabled="busy !== ''" @click="reapply(device.device)">
            {{ busy === `repair-${device.device}` ? t('probeRunning') : t('reapply') }}
          </button>
        </div>
      </div>
      <p v-if="repair" class="probe__hint" :class="repair.ok ? '' : 'probe__hint--bad'">
        {{ repair.ok ? `✓ ${repair.detail}` : `✗ ${repair.detail}` }}
      </p>
      <p class="probe__hint">{{ t('reapplyHint') }}</p>
    </section>
  </Panel>
</template>

<style scoped>
.probe {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.probe__row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.probe__repairs {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.probe__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.probe__hint--bad {
  color: var(--danger);
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
</style>
