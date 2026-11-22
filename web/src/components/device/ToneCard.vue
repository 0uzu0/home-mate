<script setup lang="ts">
/**
 * Sound style (M6c-2). Lives on the DEVICE page: it is a property of this
 * speaker, not of the conversation.
 *
 * The voicing is fixed — EchoMuse's biscuit numbers: the driver correction
 * measured on this unit, its bass guard (the stock MBCL band 1) and three
 * compressors. On top sits a ten-band graphic EQ the user owns: octave
 * centres 31.5 Hz … 16 kHz, Q 1.4, ±12 dB in 0.5 dB steps.
 *
 * The three presets are fits of the curves we tuned by ear on 2026-11-14
 * (a 3-band shape) onto these ten octave bands: RMS error 0.4–0.5 dB, worst
 * point ~1.3 dB, so 均衡 sounds like the setting the user approved.
 */
import { computed, ref, watch } from 'vue'
import { getAudioEq, putAudioEq, type EqBandConfig } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'
import GraphicEq from './GraphicEq.vue'

type Preset = 'balanced' | 'treble' | 'bass' | 'custom'

/** Octave centres of the graphic EQ (the firmware's peaking bands at Q 1.4). */
const BANDS = [31.5, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000]
const Q = 1.4

/** Fit of the ear-tuned 3-band shapes onto these ten bands. */
const PRESETS: Record<Exclude<Preset, 'custom'>, number[]> = {
  balanced: [0, -1, -4.5, -4.5, -1, -0.5, 1, 4, 2, 0.5],
  treble: [-0.5, -1, -6, -6, -1, -0.5, 1.5, 6, 4, 0.5],
  bass: [0, -0.5, -1.5, -1.5, -0.5, 0, 0.5, 2, 0, 0],
}

const gains = ref<number[]>(BANDS.map(() => 0))
const loaded = ref(false)
const loadError = ref<string | null>(null)
/** The config as loaded: the curve and any band this editor does not show are
 * carried back on save, so opening the card cannot silently rewrite them. */
const baseCurve = ref<string>('echomuse')
const offGrid = ref<EqBandConfig[]>([])
/** Q as loaded per grid frequency: an unchanged save must not rewrite it
 * (the stored config may be a 3-band shape with Q 0.6/1.2/1.0). */
const originalQ = ref<Record<number, number>>({})
const { busy, message, tone, run } = useSave()

const preset = computed<Preset>(() => {
  for (const key of Object.keys(PRESETS) as Array<Exclude<Preset, 'custom'>>) {
    const p = PRESETS[key]
    if (p.every((v, i) => Math.abs(v - (gains.value[i] ?? 0)) < 0.01)) return key
  }
  return 'custom'
})

function applyPreset(key: Exclude<Preset, 'custom'>): void {
  gains.value = PRESETS[key].slice()
  // A preset IS the user curve: hidden off-grid bands from an older shape
  // must not be re-sent (they made Balanced 11 bands and Treble 12, which the
  // API rejects — audit 2026-11-14).
  offGrid.value = []
  originalQ.value = {}
}

function toBands(values: number[]): EqBandConfig[] {
  return BANDS.map((freq, i) => ({
    freq,
    gain_db: values[i] ?? 0,
    q: originalQ.value[freq] ?? Q,
  })).filter((b) => b.gain_db !== 0)
}

function fromBands(bands: EqBandConfig[]): number[] {
  return BANDS.map((freq) => {
    const hit = bands.find((b) => Math.abs(b.freq - freq) < 1)
    return hit ? hit.gain_db : 0
  })
}

async function load(): Promise<void> {
  try {
    const cfg = await getAudioEq()
    gains.value = fromBands(cfg.bands)
    baseCurve.value = cfg.curve
    originalQ.value = Object.fromEntries(
      cfg.bands.filter((b) => BANDS.some((f) => Math.abs(b.freq - f) < 1)).map((b) => [b.freq, b.q]),
    )
    offGrid.value = cfg.bands
      .filter((b) => !BANDS.some((f) => Math.abs(b.freq - f) < 1))
      .map((b) => ({ ...b }))
    loaded.value = true
    loadError.value = null
  } catch (e) {
    // Without this the card sat on "Loading" forever (audit 2026-11-14).
    loadError.value = e instanceof Error ? e.message : String(e)
    throw e
  }
}

function retry(): void {
  loadError.value = null
  loaded.value = false
  void load().catch(() => undefined)
}

watch(
  () => loaded.value,
  () => {
    if (!loaded.value) void load().catch(() => undefined)
  },
  { immediate: true },
)

async function save(): Promise<void> {
  await run(async () => {
    const res = await putAudioEq({
      curve: baseCurve.value as never,
      bands: [...toBands(gains.value), ...offGrid.value],
    })
    await load()
    // HTTP 200 can still mean "saved, not applied" (no device, or the push
    // failed). The card must not show a green tick for that.
    const failed = (res.errors ?? []) as Array<{ device?: string; error?: string }>
    if (res.state !== 'applied' || failed.length) {
      const why = failed.map((e) => `${e.device ?? '?'}: ${e.error ?? '?'}`).join('; ')
      throw new Error(`${t('toneSavedNotApplied')}${why ? ` — ${why}` : ''}`)
    }
  })
}

function reset(): void {
  applyPreset('balanced')
}
</script>

<template>
  <Panel icon="volume" apply="immediate" :title="t('toneTitle')" :hint="t('toneHint')">
    <template #actions>
      <!-- One tone for the whole household (user decision 2026-11-14): the
           card must not read as a property of the selected speaker. -->
      <Pill size="sm" tone="neutral">{{ t('toneGlobal') }}</Pill>
    </template>
    <ErrorNotice v-if="loadError" :error="loadError" @retry="retry" />
    <EmptyState v-else-if="!loaded" :title="t('loading')" />
    <template v-else>
      <div class="tone__presets">
        <button
          v-for="key in (['balanced', 'treble', 'bass'] as const)"
          :key="key"
          type="button"
          class="tone__preset"
          :class="{ 'tone__preset--on': preset === key }"
          @click="applyPreset(key)"
        >
          {{ key === 'balanced' ? t('tonePresetBalanced') : key === 'treble' ? t('tonePresetTreble') : t('tonePresetBass') }}
        </button>
        <button type="button" class="tone__preset" :class="{ 'tone__preset--on': preset === 'custom' }" disabled>
          {{ t('tonePresetCustom') }}
        </button>
        <span class="tone__spacer"></span>
        <button type="button" class="btn" :disabled="busy" @click="reset">{{ t('toneReset') }}</button>
      </div>

      <GraphicEq v-model="gains" :bands="BANDS" :disabled="busy" />

      <p class="tone__note">{{ t('toneNote') }}</p>

      <div class="tone__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          {{ busy ? t('saving') : t('toneSave') }}
        </button>
        <span v-if="message" class="tone__msg" :class="`tone__msg--${tone}`">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.tone__presets {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
  align-items: center;
}

.tone__preset {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.28rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.tone__preset--on {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.tone__preset:disabled {
  cursor: default;
  opacity: 0.85;
}

.tone__spacer {
  flex: 1 1 auto;
}

.tone__note {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.tone__actions {
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

.tone__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.tone__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
