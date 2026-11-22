<script setup lang="ts">
/**
 * Audio gates: VAD thresholds, post-play blind window and media trim. They are
 * one mode object (one PUT), but each group saves only its own fields so a
 * change record says exactly what changed.
 */
import { computed, ref, watch } from 'vue'
import { putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import NumberField from '../NumberField.vue'
import Slider from '../Slider.vue'
import SectionLabel from '../SectionLabel.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const vadStart = ref<number | null>(null)
const vadEnd = ref<number | null>(null)
const tailBlind = ref<number | null>(null)
const mediaTrim = ref<number | null>(null)
const { busy, message, tone, run } = useSave()

/** The live detector view for THIS device (absent until it has been fed). */
const micVad = computed(() => props.device?.mic?.vad ?? null)
const closeCounts = computed(() => {
  const c = props.device?.mic?.counters
  if (!c || (c.close_quiet === undefined && c.close_cap === undefined)) return ''
  return t('vadCloseCounts')
    .replace('{quiet}', String(c.close_quiet ?? 0))
    .replace('{cap}', String(c.close_cap ?? 0))
})

function fmtDb(v: number | null | undefined): string {
  return v === null || v === undefined ? '—' : `${Math.round(v * 10) / 10} dB`
}

watch(
  () => props.mode,
  (mode) => {
    if (!mode) return
    const vad = mode.devices?.[0]?.mic?.vad ?? mode.vad
    if (vad && vadStart.value === null) {
      vadStart.value = vad.start_db
      vadEnd.value = vad.end_db
    }
    if (tailBlind.value === null && mode.tail_blind_ms !== undefined) {
      tailBlind.value = mode.tail_blind_ms
    }
    if (mediaTrim.value === null && mode.media_trim_db !== undefined) {
      mediaTrim.value = mode.media_trim_db
    }
  },
  { immediate: true },
)

async function saveVad(): Promise<void> {
  if (!props.device || vadStart.value === null || vadEnd.value === null) return
  await run(async () => {
    await putMode(props.device!.device, {
      vad: { start_db: vadStart.value!, end_db: vadEnd.value! },
    })
    await props.refresh()
  })
}

async function saveTailBlind(): Promise<void> {
  if (!props.device || tailBlind.value === null) return
  await run(async () => {
    await putMode(props.device!.device, { tail_blind_ms: tailBlind.value! })
    await props.refresh()
  })
}

async function saveTrim(): Promise<void> {
  if (!props.device || mediaTrim.value === null) return
  await run(async () => {
    await putMode(props.device!.device, { media_trim_db: mediaTrim.value! })
    await props.refresh()
  })
}
</script>

<template>
  <Panel
    icon="audioGate" apply="immediate" :title="t('audioGateTitle')" :hint="t('audioGateHint')">
    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <section class="gate">
        <SectionLabel :text="t('vadTitle')" />
        <p class="gate__hint">{{ t('vadHint') }}</p>
        <p class="gate__hint">{{ t('vadAdaptiveHint') }}</p>
        <div class="gate__row">
          <NumberField v-if="vadStart !== null" v-model="vadStart" :label="t('vadStart')" :min="-80" :max="0" unit="dBFS" />
          <NumberField v-if="vadEnd !== null" v-model="vadEnd" :label="t('vadEnd')" :min="-80" :max="0" unit="dBFS" />
          <button type="button" class="btn" :disabled="busy" @click="saveVad">{{ t('vadSave') }}</button>
          <span v-if="mode?.vad" class="gate__current mono">
            {{ t('current') }}: {{ mode.vad.start_db }} / {{ mode.vad.end_db }}
          </span>
        </div>
        <!-- R2: what the detector is ACTUALLY using. When the room is louder
             than the calibration assumed these differ from the values above;
             showing both is what makes a drifting floor visible in a second
             instead of a log dig (device 2026-11-13). -->
        <div v-if="micVad" class="gate__telemetry mono">
          <span>{{ t('vadEffective') }}: {{ fmtDb(micVad.effective_start_db) }} / {{ fmtDb(micVad.effective_end_db) }}</span>
          <span>{{ t('vadFloor') }}: {{ fmtDb(micVad.noise_db) }}</span>
          <span v-if="micVad.last_close">
            {{ t('vadLastClose') }}:
            {{ micVad.last_close.reason === 'cap' ? t('vadCloseCap') : t('vadCloseQuiet') }}
            ({{ micVad.last_close.blocks }})
          </span>
          <span v-if="closeCounts">{{ closeCounts }}</span>
        </div>
      </section>

      <section class="gate">
        <SectionLabel :text="t('tailBlindTitle')" />
        <p class="gate__hint">{{ t('tailBlindHint') }}</p>
        <div class="gate__row">
          <NumberField v-if="tailBlind !== null" v-model="tailBlind" :label="t('tailBlindMs')" :min="0" :max="2000" unit="ms" />
          <button type="button" class="btn" :disabled="busy" @click="saveTailBlind">{{ t('tailBlindSave') }}</button>
          <span v-if="mode?.tail_blind_ms !== undefined" class="gate__current mono">
            {{ t('current') }}: {{ mode.tail_blind_ms }} ms
          </span>
        </div>
      </section>

      <section class="gate">
        <SectionLabel :text="t('mediaTrimTitle')" />
        <p class="gate__hint">{{ t('mediaTrimHint') }}</p>
        <div class="gate__row">
          <Slider
            v-if="mediaTrim !== null"
            v-model="mediaTrim"
            :label="t('mediaTrimDb')"
            :min="-12"
            :max="0"
            :step="1"
            unit="dB"
          />
          <button type="button" class="btn" :disabled="busy" @click="saveTrim">{{ t('mediaTrimSave') }}</button>
          <span v-if="mode?.media_trim_db !== undefined" class="gate__current mono">
            {{ t('current') }}: {{ mode.media_trim_db }} dB
          </span>
        </div>
      </section>

      <p v-if="message" class="gate__msg" :class="`gate__msg--${tone}`">{{ message }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.gate {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.gate__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
  max-width: 70ch;
}

.gate__row {
  display: flex;
  align-items: end;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.gate__current {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.gate__telemetry {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2) var(--sp-3);
  font-size: var(--fs-xs);
  color: var(--muted);
  border-top: 1px dashed var(--line);
  padding-top: var(--sp-2);
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

.gate__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.gate__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
