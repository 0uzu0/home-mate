<script setup lang="ts">
/**
 * Contract v2 #4 (2026-11-16): the device spotter's sensitivity, as the
 * household's own setting.
 *
 * Two numbers because the model has two knobs: `threshold` (lower = fires on
 * weaker audio) and `score` (higher = stronger keyword boost). The defaults
 * are sherpa's standard operating point with a 3.0 boost — the values this
 * fleet has run since the 2026-11-15 false-wake fix.
 *
 * Saving writes the CONFIG on the same validated management path as every
 * other voice setting. The DEVICE only changes on a deploy, so this card
 * offers "save and deploy" and shows what the loaded model reports it is
 * running with (`kws.threshold/score`) instead of pretending a save applied.
 */
import { computed, ref, watch } from 'vue'
import { deployReflex, putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import NumberField from '../NumberField.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const threshold = ref(0.25)
const score = ref(3.0)
const touched = ref(false)
const { busy, message, tone, run } = useSave()

watch(
  () => props.mode,
  (mode) => {
    if (!mode || touched.value) return
    threshold.value = mode.kws_threshold ?? 0.25
    score.value = mode.kws_score ?? 3.0
  },
  { immediate: true },
)

/** What the loaded spotter says — a device fact, not our intent. */
const applied = computed(() => {
  const k = props.device?.kws
  if (!k || !k.threshold) return null
  return { threshold: k.threshold, score: k.score ?? 0 }
})

const drift = computed(() => {
  const a = applied.value
  if (!a) return false
  return Math.abs(a.threshold - threshold.value) > 0.001 || Math.abs((a.score ?? 0) - score.value) > 0.01
})

function round(v: number, digits: number): number {
  const f = 10 ** digits
  return Math.round(v * f) / f
}

async function save(deploy: boolean): Promise<void> {
  if (!props.device) return
  const patch = { kws_threshold: round(threshold.value, 2), kws_score: round(score.value, 1) }
  await run(async () => {
    await putMode(props.device!.device, patch)
    if (deploy) await deployReflex({ device: props.device!.device })
    touched.value = false
    await props.refresh()
    return deploy ? t('kwsSavedDeployed') : t('kwsSavedConfig')
  })
}
</script>

<template>
  <Panel icon="kws" apply="restart" :title="t('kwsSensitivityTitle')" :hint="t('kwsSensitivityHint')">
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
    <p class="sens__note">{{ t('kwsSensitivityApplyNote') }}</p>
    <p v-if="applied" class="sens__applied">
      <Pill size="sm" :tone="drift ? 'warn' : 'ok'">
        {{ t('kwsApplied', { threshold: applied.threshold.toFixed(2), score: (applied.score ?? 0).toFixed(2) }) }}
      </Pill>
      <span v-if="drift" class="sens__drift">{{ t('kwsAppliedDrift') }}</span>
    </p>
    <div class="sens__actions">
      <button type="button" class="sens__save" :disabled="busy || !device || !mode" @click="save(false)">
        {{ busy ? t('saving') : t('kwsSaveConfig') }}
      </button>
      <button type="button" class="sens__deploy" :disabled="busy || !device || !mode" @click="save(true)">
        {{ t('kwsSaveDeploy') }}
      </button>
      <span :class="`sens__${tone}`" role="status">{{ message }}</span>
    </div>
  </Panel>
</template>

<style scoped>
.sens__note { color: var(--muted); font-size: var(--fs-sm); }
.sens__applied { display: flex; align-items: center; gap: var(--sp-2); flex-wrap: wrap; font-size: var(--fs-sm); }
.sens__drift { color: var(--warn); }
.sens__actions { display: flex; align-items: center; gap: var(--sp-2); flex-wrap: wrap; }
.sens__save { border: 1px solid var(--line); border-radius: var(--r-sm); background: transparent; color: var(--ink); padding: 0.24rem var(--sp-3); cursor: pointer; }
.sens__deploy { border: 1px solid var(--accent); border-radius: var(--r-sm); background: var(--accent); color: var(--on-accent); padding: 0.24rem var(--sp-3); cursor: pointer; }
.sens__save:disabled, .sens__deploy:disabled { opacity: 0.5; cursor: not-allowed; }
.sens__ok { color: var(--ok); }
.sens__error { color: var(--danger); }
</style>
