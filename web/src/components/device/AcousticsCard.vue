<script setup lang="ts">
/**
 * Per-unit acoustics & microphone calibration card (#113 / A1).
 *
 * Reads and writes `/api/v0/devices/:id/profile` (`acoustic` block) and displays
 * the honest Gateway `acoustics_state` / `acoustics_receipt` (never reporting
 * "applied" until the Gateway returns an `active` receipt).
 */
import { computed, ref, watch } from 'vue'
import {
  getDeviceProfile,
  putDeviceProfile,
  type AcousticsReceipt,
  type ModeDevice,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useResource } from '../../hooks/useResource'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = defineProps<{
  device: ModeDevice
}>()

const profile = useResource((signal) => getDeviceProfile(props.device.device, { signal }), {
  timeoutMs: 5000,
  retries: 1,
})

const volumeCap = ref(0.85)
const mixGain = ref(1.0)
const vadThreshold = ref(0.5)
const onsetDbfs = ref<string>('')
const micOffsetDb = ref(0.0)
const micCalibrated = ref(false)
const acousticsState = ref<string>('')
const receipt = ref<AcousticsReceipt | null>(null)
const saving = ref(false)
const message = ref('')
const tone = ref<'ok' | 'error'>('ok')
const busy = computed(() => profile.loading.value || saving.value)

const badgeTone = computed<'ok' | 'warn' | 'danger' | 'muted'>(() => {
  if (acousticsState.value === 'applied' || receipt.value?.state === 'active') return 'ok'
  if (acousticsState.value === 'failed' || receipt.value?.state === 'failed') return 'danger'
  if (acousticsState.value === 'saved_pending' || acousticsState.value === 'pending') return 'warn'
  return 'muted'
})

const badgeLabel = computed(() => {
  if (acousticsState.value === 'applied' || receipt.value?.state === 'active') {
    const gen = receipt.value?.block_generation
    return gen ? `${t('acousticsStateApplied')} (gen ${gen})` : t('acousticsStateApplied')
  }
  if (acousticsState.value === 'failed' || receipt.value?.state === 'failed') {
    const detail = receipt.value?.reason ?? receipt.value?.error_code
    return detail ? `${t('acousticsStateFailed')}: ${detail}` : t('acousticsStateFailed')
  }
  if (acousticsState.value === 'saved_pending' || acousticsState.value === 'pending') {
    return t('acousticsStatePending')
  }
  return ''
})

watch(
  () => profile.data.value,
  (res) => {
    if (!res) return
    volumeCap.value = res.acoustic.volume_cap
    mixGain.value = res.acoustic.mix_gain
    vadThreshold.value = res.acoustic.vad_threshold
    onsetDbfs.value =
      res.acoustic.onset_threshold_dbfs !== undefined &&
      res.acoustic.onset_threshold_dbfs !== null
        ? String(res.acoustic.onset_threshold_dbfs)
        : ''
    micOffsetDb.value = res.acoustic.mic_gain_offset_db ?? 0.0
    micCalibrated.value = Boolean(res.acoustic.mic_calibrated)
    acousticsState.value = res.acoustics_state ?? ''
    receipt.value = res.acoustics_receipt ?? null
  },
  { immediate: true },
)

async function save(): Promise<void> {
  saving.value = true
  message.value = ''
  try {
    const acoustic: Record<string, number | boolean> = {
      volume_cap: Number(volumeCap.value),
      mix_gain: Number(mixGain.value),
      vad_threshold: Number(vadThreshold.value),
      mic_gain_offset_db: Number(micOffsetDb.value),
      mic_calibrated: micCalibrated.value,
    }
    const trimmedOnset = String(onsetDbfs.value ?? '').trim()
    if (trimmedOnset !== '' && Number.isFinite(Number(trimmedOnset))) {
      acoustic.onset_threshold_dbfs = Number(trimmedOnset)
    }
    const res = await putDeviceProfile(props.device.device, { acoustic })
    volumeCap.value = res.acoustic.volume_cap
    mixGain.value = res.acoustic.mix_gain
    vadThreshold.value = res.acoustic.vad_threshold
    onsetDbfs.value =
      res.acoustic.onset_threshold_dbfs !== undefined &&
      res.acoustic.onset_threshold_dbfs !== null
        ? String(res.acoustic.onset_threshold_dbfs)
        : ''
    micOffsetDb.value = res.acoustic.mic_gain_offset_db ?? 0.0
    micCalibrated.value = Boolean(res.acoustic.mic_calibrated)
    acousticsState.value = res.acoustics_state ?? res.state ?? ''
    receipt.value = (res.acoustics_receipt as AcousticsReceipt | null | undefined) ?? null
    tone.value = acousticsState.value === 'failed' ? 'error' : 'ok'
    message.value = badgeLabel.value || res.state
  } catch (err) {
    tone.value = 'error'
    message.value = errorDetail(err)
  } finally {
    saving.value = false
  }
}

watch(
  () => props.device.device,
  () => {
    message.value = ''
    profile.reset()
    void profile.load()
  },
  { immediate: true },
)
</script>

<template>
  <Panel icon="mic" :title="t('acousticsTitle')" :hint="t('acousticsHint')">
    <template v-if="badgeLabel" #actions>
      <Pill :tone="badgeTone">{{ badgeLabel }}</Pill>
    </template>

    <LoadingBlock v-if="profile.loading.value && !profile.data.value" :label="t('loading')" />
    <ErrorNotice
      v-else-if="profile.error.value && !profile.data.value"
      :error="profile.error.value"
      @retry="profile.load()"
    />
    <template v-else>
      <div class="ac__grid">
        <label class="ac__field">
          <span class="ac__label">{{ t('acousticsVolumeCap') }}</span>
          <input v-model.number="volumeCap" type="number" step="0.01" min="0" max="1" :disabled="busy" />
        </label>
        <label class="ac__field">
          <span class="ac__label">{{ t('acousticsMixGain') }}</span>
          <input v-model.number="mixGain" type="number" step="0.05" min="0.05" max="2" :disabled="busy" />
        </label>
        <label class="ac__field">
          <span class="ac__label">{{ t('acousticsOnsetDbfs') }}</span>
          <input v-model="onsetDbfs" type="number" step="1" min="-120" max="0" placeholder="-45" :disabled="busy" />
        </label>
        <label class="ac__field">
          <span class="ac__label">{{ t('acousticsMicOffsetDb') }}</span>
          <input v-model.number="micOffsetDb" type="number" step="0.01" min="-24" max="24" :disabled="busy" />
        </label>
      </div>

      <Toggle v-model="micCalibrated" :label="t('acousticsMicCalibrated')" :disabled="busy" />

      <div class="ac__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          {{ t('acousticsSave') }}
        </button>
        <span v-if="message" :class="['ac__msg', `ac__msg--${tone}`]" role="status">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.ac__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: var(--sp-3);
  margin-bottom: var(--sp-3);
}
.ac__field {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}
.ac__label {
  font-size: var(--fs-xs);
  color: var(--muted);
}
.ac__field input {
  padding: 6px 8px;
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--bg);
  color: var(--ink);
  font-family: var(--font-mono);
}
.ac__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  margin-top: var(--sp-3);
}
.ac__msg {
  font-size: var(--fs-sm);
}
.ac__msg--ok {
  color: var(--ok);
}
.ac__msg--error {
  color: var(--danger);
}
</style>
