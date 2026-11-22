<script setup lang="ts">
/**
 * Panel 1: Wake & Response (唤醒与响应)
 *
 * Unifies wake words, 3-level simplified sensitivity presets (with raw
 * threshold/score behind advancedMode), wake ack phrases, and idle speech
 * fallback into a single panel with one Save & Apply action that deploys
 * automatically.
 */
import { computed, ref, watch } from 'vue'
import { deployReflex, putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { advancedMode } from '../../stores/advanced'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import PhraseList from '../PhraseList.vue'
import Toggle from '../Toggle.vue'
import NumberField from '../NumberField.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

type SensitivityPreset = 'low' | 'standard' | 'high' | 'custom'

const PRESETS: Record<Exclude<SensitivityPreset, 'custom'>, { threshold: number; score: number }> = {
  low: { threshold: 0.32, score: 2.5 },
  standard: { threshold: 0.25, score: 3.0 },
  high: { threshold: 0.18, score: 3.5 },
}

const wakeWords = ref<string[]>([])
const threshold = ref(0.25)
const score = ref(3.0)
const wakeAckEnabled = ref(true)
const wakeAckPhrases = ref<string[]>([])
const asrFallback = ref(true)
const touched = ref(false)

const { busy, message, tone, run } = useSave()

watch(
  [() => props.mode, () => props.device],
  ([mode, device]) => {
    if (!mode || touched.value) return
    wakeWords.value = [...(mode.wake_words ?? [])]
    threshold.value = mode.kws_threshold ?? 0.25
    score.value = mode.kws_score ?? 3.0
    asrFallback.value = mode.asr_wake_fallback !== false
    if (device?.wake_ack) {
      wakeAckEnabled.value = device.wake_ack.enabled !== false
      wakeAckPhrases.value = [...(device.wake_ack.phrases ?? [])]
    }
  },
  { immediate: true },
)

const activePreset = computed<SensitivityPreset>(() => {
  for (const key of ['low', 'standard', 'high'] as const) {
    const p = PRESETS[key]
    if (Math.abs(threshold.value - p.threshold) < 0.015 && Math.abs(score.value - p.score) < 0.15) {
      return key
    }
  }
  return 'custom'
})

function selectPreset(preset: Exclude<SensitivityPreset, 'custom'>): void {
  touched.value = true
  threshold.value = PRESETS[preset].threshold
  score.value = PRESETS[preset].score
}

function round(v: number, digits: number): number {
  const f = 10 ** digits
  return Math.round(v * f) / f
}

const appliedKws = computed(() => {
  const k = props.device?.kws
  if (!k || !k.threshold) return null
  return { threshold: k.threshold, score: k.score ?? 0 }
})

async function saveAndDeploy(resynth = false): Promise<void> {
  if (!props.device) return
  await run(async () => {
    await putMode(props.device!.device, {
      wake_words: wakeWords.value,
      kws_threshold: round(threshold.value, 2),
      kws_score: round(score.value, 1),
      asr_wake_fallback: asrFallback.value,
      wake_ack: {
        enabled: wakeAckEnabled.value,
        phrases: wakeAckPhrases.value,
        ...(resynth ? { resynth: true } : {}),
      },
    })
    await deployReflex({ device: props.device!.device })
    touched.value = false
    await props.refresh()
    return resynth ? t('resynthQueued') : t('wakePanelSaved')
  })
}
</script>

<template>
  <Panel icon="wake" apply="immediate" :title="t('panelWakeTitle')" :hint="t('panelWakeHint')">
    <template #actions>
      <Pill size="sm" :tone="asrFallback ? 'neutral' : 'warn'">
        {{ asrFallback ? t('wakePathAsr') : t('wakePathAsrOff') }}
      </Pill>
    </template>

    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <div class="wp__block">
        <PhraseList
          v-model="wakeWords"
          :label="t('wakeTitle')"
          :placeholder="t('wakePlaceholder')"
          :maxlength="16"
          :disabled="busy"
          :empty-label="t('wakeEmpty')"
          @update:model-value="touched = true"
        />
        <p class="wp__hint">{{ t('wakeLocalHint') }}</p>
      </div>

      <div class="wp__block">
        <span class="wp__label">{{ t('kwsPresetLabel') }}</span>
        <div class="wp__presets" role="group" :aria-label="t('kwsPresetLabel')">
          <button
            v-for="preset in (['low', 'standard', 'high'] as const)"
            :key="preset"
            type="button"
            class="wp__preset"
            :class="{ 'wp__preset--active': activePreset === preset }"
            :disabled="busy"
            @click="selectPreset(preset)"
          >
            <strong>{{ t(`kwsPreset_${preset}`) }}</strong>
            <span>{{ t(`kwsPreset_${preset}_hint`) }}</span>
          </button>
        </div>

        <div v-if="advancedMode" class="wp__raw">
          <NumberField
            v-model="threshold"
            :label="t('kwsThreshold')"
            :min="0.05"
            :max="0.9"
            :disabled="busy"
            :hint="t('kwsThresholdHint')"
            @update:model-value="touched = true"
          />
          <NumberField
            v-model="score"
            :label="t('kwsScore')"
            :min="0.5"
            :max="6"
            :disabled="busy"
            :hint="t('kwsScoreHint')"
            @update:model-value="touched = true"
          />
          <Pill v-if="appliedKws" size="sm" tone="ok" mono>
            {{ t('kwsApplied', { threshold: appliedKws.threshold.toFixed(2), score: appliedKws.score.toFixed(2) }) }}
          </Pill>
        </div>
      </div>

      <div class="wp__block">
        <Toggle
          v-model="wakeAckEnabled"
          :label="t('wakeAckEnabled')"
          :hint="t('wakeAckHint')"
          :disabled="busy"
          @update:model-value="touched = true"
        />
        <PhraseList
          v-model="wakeAckPhrases"
          :label="t('wakeAckTitle')"
          :placeholder="t('wakeAckPlaceholder')"
          :maxlength="6"
          :disabled="busy"
          @update:model-value="touched = true"
        />
      </div>

      <div class="wp__block">
        <Toggle
          v-model="asrFallback"
          :label="t('asrFallbackEnabled')"
          :disabled="busy"
          @update:model-value="touched = true"
        />
        <p class="wp__note" :class="asrFallback ? 'wp__note--warn' : 'wp__note--ok'">
          {{ asrFallback ? t('asrFallbackOnNote') : t('asrFallbackOffNote', { seconds: (mode?.follow_window_ms ?? 20000) / 1000 }) }}
        </p>
      </div>

      <div class="wp__actions">
        <button
          type="button"
          class="btn btn--primary"
          :disabled="busy || !wakeWords.length"
          @click="saveAndDeploy(false)"
        >
          {{ busy ? t('saving') : t('wakePanelSave') }}
        </button>
        <button type="button" class="btn" :disabled="busy" @click="saveAndDeploy(true)">
          {{ t('resynth') }}
        </button>
        <span v-if="message" class="wp__msg" :class="`wp__msg--${tone}`" role="status">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.wp__block {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.wp__label {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--ink-2);
}

.wp__hint {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.wp__presets {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(10rem, 1fr));
  gap: var(--sp-2);
}

.wp__preset {
  display: flex;
  flex-direction: column;
  gap: 0.15rem;
  text-align: left;
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  background: var(--surface);
  color: var(--ink);
  cursor: pointer;
}

.wp__preset strong {
  font-size: var(--fs-sm);
}

.wp__preset span {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wp__preset--active {
  border-color: var(--accent);
  background: var(--accent-soft);
}

.wp__raw {
  display: flex;
  flex-wrap: wrap;
  align-items: end;
  gap: var(--sp-3);
  margin-top: var(--sp-1);
}

.wp__note {
  font-size: var(--fs-sm);
  line-height: 1.5;
  border-left: 3px solid var(--line-strong);
  padding-left: var(--sp-2);
}

.wp__note--warn {
  border-left-color: var(--warn);
}

.wp__note--ok {
  border-left-color: var(--ok);
}

.wp__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.wp__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.wp__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
