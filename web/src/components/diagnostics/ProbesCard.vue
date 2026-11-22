<script setup lang="ts">
/**
 * Active probes and repairs. Everything here has a real side effect (sound,
 * re-sent state), so each action is a separate button with its own honest
 * result — no probe is fired automatically on page load.
 */
import { ref } from 'vue'
import {
  reapplyModeRepair,
  runPlayAssetTest,
  runPlaybackLoopbackTest,
  runVoicePlayTest,
  type LoopbackResult,
  type ModeDevice,
  type PlayAssetResult,
  type VoicePlayTestResult,
} from '../../api'
import { t, type Key } from '../../i18n'
import { errorDetail, formatDuration } from '../../format'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import SectionLabel from '../SectionLabel.vue'
import EmptyState from '../EmptyState.vue'

defineProps<{ devices: ModeDevice[] }>()

type ProbeState = { ok: boolean; detail: string }

const voice = ref<ProbeState | null>(null)
const asset = ref<ProbeState | null>(null)
const loopback = ref<ProbeState | null>(null)
const repair = ref<ProbeState | null>(null)
const busy = ref('')

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
  busy.value = 'voice'
  voice.value = null
  try {
    const result: VoicePlayTestResult = await runVoicePlayTest()
    voice.value = result.ok
      ? {
          ok: true,
          detail: `${result.device ?? ''} · ${result.stage ?? ''} · ${formatDuration(result.synth_ms)} / ${formatDuration(result.total_ms)} · ${result.provider ?? ''} ${result.voice ?? ''}`,
        }
      : { ok: false, detail: result.error || result.stage || 'failed' }
  } catch (err) {
    voice.value = { ok: false, detail: errorDetail(err) }
  } finally {
    busy.value = ''
  }
}

async function runAsset(): Promise<void> {
  busy.value = 'asset'
  asset.value = null
  try {
    const result: PlayAssetResult = await runPlayAssetTest('erle-stimulus.wav')
    asset.value = result.ok
      ? {
          ok: true,
          detail: `${result.device ?? ''} · ${result.asset ?? ''} · ${result.stage ?? ''} · ${formatDuration(result.duration_ms)} / ${formatDuration(result.total_ms)}`,
        }
      : { ok: false, detail: result.error || result.stage || 'failed' }
  } catch (err) {
    asset.value = { ok: false, detail: errorDetail(err) }
  } finally {
    busy.value = ''
  }
}

async function runLoopback(): Promise<void> {
  busy.value = 'loopback'
  loopback.value = null
  try {
    const result = await runPlaybackLoopbackTest()
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
    loopback.value = { ok: false, detail: errorDetail(err) }
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
