<script setup lang="ts">
/**
 * Panel 3: Interrupt & Reply Style (打断与播报风格)
 *
 * Unifies interrupt/stop/continue words (with auto-deploy; token compiler
 * details strictly behind advancedMode), spoken length/detail controls +
 * truncation notice, and waiting filler phrases.
 */
import { computed, ref, watch } from 'vue'
import {
  getReflex,
  getReflexTokens,
  putMode,
  putReflex,
  type FillerMode,
  type FillerScene,
  type ModeDevice,
  type ModeSnapshot,
  type ReflexWord,
} from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import { advancedMode } from '../../stores/advanced'
import Panel from '../Panel.vue'
import Toggle from '../Toggle.vue'
import TextInput from '../TextInput.vue'
import Select from '../Select.vue'
import PhraseList from '../PhraseList.vue'
import NumberField from '../NumberField.vue'
import Slider from '../Slider.vue'
import SectionLabel from '../SectionLabel.vue'
import EmptyState from '../EmptyState.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

// ── Interrupt & stop/continue words ─────────────────────────────────────
const reflex = useResource((signal) => getReflex({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => reflex.load(), 15000)

const keywordInterrupt = ref(true)
const reflexWords = ref<ReflexWord[] | null>(null)
const stopEnabled = ref(true)
const stopWords = ref<string[]>([])
const continueWords = ref<string[]>([])
const tokenError = ref('')
const interruptTouched = ref(false)
const autoTokens = ref<Set<number>>(new Set())
let tokenTimer: number | null = null

const {
  busy: intBusy,
  message: intMsg,
  tone: intTone,
  run: runInt,
} = useSave()

// ── Spoken length, truncation, & filler cues ────────────────────────────
const sentences = ref(2)
const chars = ref(120)
const detailSentences = ref(6)
const detailChars = ref(300)
const backstopSentences = ref(400)
const backstopChars = ref(2000)
const continueHint = ref('')
const resumeLead = ref('')
const truncationNotice = ref('')

type Scenes = Record<FillerScene, string[]>
const fillerDelay = ref(300)
const fillerToolDelay = ref(600)
const fillerMode = ref<FillerMode>('radio')
const fillerScenes = ref<Scenes>({ thinking: [], search: [], device: [], story: [] })
const styleTouched = ref(false)

const {
  busy: styleBusy,
  message: styleMsg,
  tone: styleTone,
  run: runStyle,
} = useSave()

const sceneKeys: FillerScene[] = ['thinking', 'search', 'device', 'story']
const sceneLabels = computed<Record<FillerScene, string>>(() => ({
  thinking: t('fillerSceneThinking'),
  search: t('fillerSceneSearch'),
  device: t('fillerSceneDevice'),
  story: t('fillerSceneStory'),
}))

const fillerModeOptions = computed(() => [
  { value: 'radio', label: t('fillerModeRadio') },
  { value: 'interrupt', label: t('fillerModeInterrupt') },
])

const intentOptions = computed(() => [
  { value: 'wake', label: t('reflexIntentWake') },
  { value: 'retreat', label: t('reflexIntentRetreat') },
  { value: 'pause', label: t('reflexIntentPause') },
  { value: 'continue', label: t('reflexIntentContinue') },
  { value: 'next', label: t('reflexIntentNext') },
  { value: 'stop', label: t('reflexIntentStop') },
])

watch(
  () => props.mode,
  (mode) => {
    if (!mode) return
    if (!interruptTouched.value) {
      keywordInterrupt.value = mode.keyword_interrupt !== false
      if (mode.stop_intents) {
        stopEnabled.value = mode.stop_intents.enabled !== false
        stopWords.value = [...mode.stop_intents.stop_words]
        continueWords.value = [...mode.stop_intents.continue_words]
      }
    }
    if (!styleTouched.value) {
      if (mode.spoken) {
        sentences.value = mode.spoken.max_sentences
        chars.value = mode.spoken.max_chars
        detailSentences.value = mode.spoken.detail_max_sentences
        detailChars.value = mode.spoken.detail_max_chars
        backstopSentences.value = mode.spoken.backstop_max_sentences
        backstopChars.value = mode.spoken.backstop_max_chars
        continueHint.value = mode.spoken.continue_hint
        resumeLead.value = mode.spoken.resume_lead ?? ''
      }
      truncationNotice.value = mode.truncation_notice ?? ''
      if (mode.filler) {
        fillerDelay.value = mode.filler.delay_ms
        fillerToolDelay.value = mode.filler.tool_delay_ms ?? 600
        fillerMode.value = (mode.filler.mode as FillerMode) || 'radio'
        fillerScenes.value = {
          thinking: [...mode.filler.thinking],
          search: [...mode.filler.search],
          device: [...mode.filler.device],
          story: [...mode.filler.story],
        }
      }
    }
  },
  { immediate: true },
)

watch(
  () => reflex.data.value,
  (state) => {
    if (state && reflexWords.value === null) {
      reflexWords.value = state.words.map((w) => ({ ...w }))
      autoTokens.value = new Set(state.words.map((_, i) => i))
    }
  },
  { immediate: true },
)

function generate(i: number): void {
  const word = reflexWords.value?.[i]
  if (!word) return
  const text = word.text.trim()
  if (!text) return
  void getReflexTokens(text)
    .then((res) => {
      const row = reflexWords.value?.[i]
      if (!row || row.text.trim() !== text) return
      if (res.error) {
        tokenError.value = res.error
        return
      }
      tokenError.value = ''
      row.tokens = res.tokens
      autoTokens.value.add(i)
    })
    .catch((err) => {
      tokenError.value = String(err)
    })
}

function onWordInput(i: number): void {
  interruptTouched.value = true
  const word = reflexWords.value?.[i]
  if (!word) return
  if (!autoTokens.value.has(i) && word.tokens.trim() !== '') return
  if (tokenTimer !== null) window.clearTimeout(tokenTimer)
  tokenTimer = window.setTimeout(() => generate(i), 300)
}

function addWord(): void {
  if (!reflexWords.value) return
  interruptTouched.value = true
  reflexWords.value.push({ text: '', tokens: '', intent: 'pause', enabled: true })
}

function removeWord(i: number): void {
  interruptTouched.value = true
  reflexWords.value?.splice(i, 1)
}

async function saveInterrupts(): Promise<void> {
  if (!props.device || !reflexWords.value) return
  const words = reflexWords.value.map((w) => ({ ...w }))
  await runInt(async () => {
    await putMode(props.device!.device, {
      keyword_interrupt: keywordInterrupt.value,
      stop_intents: {
        enabled: stopEnabled.value,
        stop_words: stopWords.value,
        continue_words: continueWords.value,
      },
    })
    const saved = await putReflex({ words, deploy: true, device: props.device!.device })
    reflexWords.value = saved.words.map((w) => ({ ...w }))
    interruptTouched.value = false
    await reflex.load()
    await props.refresh()
    return t('reflexDeployed')
  })
}

async function saveStyle(resynth = false): Promise<void> {
  if (!props.device) return
  await runStyle(async () => {
    await putMode(
      props.device!.device,
      {
        spoken: {
          max_sentences: sentences.value,
          max_chars: chars.value,
          detail_max_sentences: detailSentences.value,
          detail_max_chars: detailChars.value,
          backstop_max_sentences: backstopSentences.value,
          backstop_max_chars: backstopChars.value,
          continue_hint: continueHint.value.trim(),
          resume_lead: resumeLead.value.trim(),
        },
        truncation_notice: truncationNotice.value.trim(),
        filler: {
          delay_ms: fillerDelay.value,
          tool_delay_ms: fillerToolDelay.value,
          mode: fillerMode.value,
          thinking: fillerScenes.value.thinking,
          search: fillerScenes.value.search,
          device: fillerScenes.value.device,
          story: fillerScenes.value.story,
          ...(resynth ? { resynth: true } : {}),
        },
      },
      { timeoutMs: 12000 },
    )
    styleTouched.value = false
    await props.refresh()
    return resynth ? t('resynthQueued') : t('saved')
  })
}

const compiled = computed(() => reflex.data.value?.compiled ?? '')
const warnings = computed(() => reflex.data.value?.warnings ?? [])
</script>

<template>
  <Panel icon="spoken" apply="immediate" :title="t('panelStyleTitle')" :hint="t('panelStyleHint')">
    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <!-- Sub-section A: Interrupt & Stop/Continue Words -->
      <section class="sp__block">
        <SectionLabel :text="t('interruptTitle')" />
        <ErrorNotice v-if="reflex.error.value" :error="reflex.error.value" compact @retry="reflex.load" />
        <LoadingBlock v-else-if="!reflexWords" :label="t('loading')" />
        <template v-else>
          <Toggle
            v-model="keywordInterrupt"
            :label="t('kwsEnabled')"
            :hint="t('kwsToggleHint')"
            :disabled="intBusy"
            @update:model-value="interruptTouched = true"
          />

          <div class="sp__rows">
            <div v-for="(word, i) in reflexWords" :key="i" class="sp__word-row">
              <div @input="onWordInput(i)">
                <TextInput v-model="word.text" :label="t('reflexWord')" :maxlength="12" />
              </div>
              <Select
                v-model="word.intent"
                :label="t('reflexIntent')"
                :options="intentOptions"
                @update:model-value="interruptTouched = true"
              />
              <Toggle
                :model-value="word.enabled"
                :label="t('reflexEnabled')"
                @update:model-value="(v: boolean) => { word.enabled = v; interruptTouched = true }"
              />
              <button
                type="button"
                class="btn btn--danger btn--compact"
                :disabled="intBusy"
                :title="t('reflexRemove')"
                :aria-label="t('reflexRemove')"
                @click="removeWord(i)"
              >
                ×
              </button>
            </div>
          </div>

          <div class="sp__actions">
            <button type="button" class="btn" :disabled="intBusy" @click="addWord">{{ t('reflexAdd') }}</button>
          </div>

          <Toggle
            v-model="stopEnabled"
            :label="t('stopEnabled')"
            :disabled="intBusy"
            @update:model-value="interruptTouched = true"
          />
          <div class="sp__lists">
            <PhraseList
              v-model="stopWords"
              :label="t('stopWordsTitle')"
              :placeholder="t('phrasePlaceholder')"
              :maxlength="8"
              :disabled="intBusy"
              @update:model-value="interruptTouched = true"
            />
            <PhraseList
              v-model="continueWords"
              :label="t('continueWordsTitle')"
              :placeholder="t('phrasePlaceholder')"
              :maxlength="8"
              :disabled="intBusy"
              @update:model-value="interruptTouched = true"
            />
          </div>

          <ul v-if="warnings.length" class="sp__warnings">
            <li v-for="(w, i) in warnings" :key="i">{{ w }}</li>
          </ul>
          <p v-if="tokenError" class="sp__msg sp__msg--error">{{ tokenError }}</p>

          <details v-if="advancedMode" class="sp__details">
            <summary>{{ t('reflexDetailsSummary') }}</summary>
            <div class="sp__tokens">
              <div v-for="(word, i) in reflexWords" :key="`tok-${i}`" class="sp__token-row">
                <span class="sp__token-word">{{ word.text || '—' }}</span>
                <TextInput v-model="word.tokens" :label="t('reflexTokens')" mono />
                <button
                  type="button"
                  class="btn btn--ghost"
                  :disabled="intBusy || !word.text.trim()"
                  @click="generate(i)"
                >
                  {{ t('reflexGenerate') }}
                </button>
              </div>
              <pre class="sp__compiled mono">{{ compiled || '—' }}</pre>
            </div>
          </details>

          <div class="sp__actions">
            <button type="button" class="btn btn--primary" :disabled="intBusy" @click="saveInterrupts">
              {{ intBusy ? t('saving') : t('reflexSaveSync') }}
            </button>
            <span v-if="intMsg" class="sp__msg" :class="`sp__msg--${intTone}`" role="status">{{ intMsg }}</span>
          </div>
        </template>
      </section>

      <!-- Sub-section B: Reply Length, Truncation Notice, & Waiting Fillers -->
      <section class="sp__block">
        <SectionLabel :text="t('spokenTitle')" />
        <p class="sp__hint">{{ t('spokenHint') }}</p>
        <div class="sp__row">
          <NumberField
            v-model="sentences"
            :label="t('spokenSentences')"
            :min="1"
            :max="8"
            @update:model-value="styleTouched = true"
          />
          <NumberField
            v-model="chars"
            :label="t('spokenChars')"
            :min="40"
            :max="600"
            :unit="t('unitChars')"
            @update:model-value="styleTouched = true"
          />
          <NumberField
            v-model="detailSentences"
            :label="t('spokenDetailSentences')"
            :min="1"
            :max="8"
            @update:model-value="styleTouched = true"
          />
          <NumberField
            v-model="detailChars"
            :label="t('spokenDetailChars')"
            :min="40"
            :max="600"
            :unit="t('unitChars')"
            @update:model-value="styleTouched = true"
          />
        </div>

        <div v-if="advancedMode" class="sp__row">
          <NumberField
            v-model="backstopSentences"
            :label="t('spokenBackstopSentences')"
            :min="2"
            :max="1000"
            @update:model-value="styleTouched = true"
          />
          <NumberField
            v-model="backstopChars"
            :label="t('spokenBackstopChars')"
            :min="200"
            :max="6000"
            :unit="t('unitChars')"
            @update:model-value="styleTouched = true"
          />
        </div>

        <div class="sp__acks">
          <TextInput
            v-model="continueHint"
            :label="t('spokenContinueHint')"
            :hint="t('spokenContinueHintHint')"
            wide
            @update:model-value="styleTouched = true"
          />
          <TextInput
            v-model="resumeLead"
            :label="t('spokenResumeLead')"
            :hint="t('spokenResumeLeadHint')"
            wide
            @update:model-value="styleTouched = true"
          />
          <TextInput
            v-model="truncationNotice"
            :label="t('truncationTitle')"
            :hint="t('truncationHint')"
            :maxlength="60"
            wide
            @update:model-value="styleTouched = true"
          />
        </div>
      </section>

      <section class="sp__block">
        <SectionLabel :text="t('fillerTitle')" />
        <p class="sp__hint">{{ t('fillerHint') }}</p>
        <div class="sp__filler-controls">
          <Slider
            v-model="fillerDelay"
            :label="t('fillerDelay')"
            :min="200"
            :max="10000"
            :step="100"
            :format="(value) => `${value} ms`"
            :hint="t('fillerDelayUnit')"
            @update:model-value="styleTouched = true"
          />
          <Slider
            v-model="fillerToolDelay"
            :label="t('fillerToolDelay')"
            :min="0"
            :max="5000"
            :step="100"
            :format="(value) => `${value} ms`"
            :hint="t('fillerToolDelayUnit')"
            @update:model-value="styleTouched = true"
          />
          <Select
            v-model="fillerMode"
            :label="t('fillerMode')"
            :options="fillerModeOptions"
            @update:model-value="styleTouched = true"
          />
        </div>

        <div class="sp__scenes">
          <div v-for="scene in sceneKeys" :key="scene" class="sp__scene">
            <PhraseList
              v-model="fillerScenes[scene]"
              :label="sceneLabels[scene]"
              :placeholder="t('fillerPlaceholder')"
              :maxlength="40"
              :disabled="styleBusy"
              :empty-label="t('fillerOff')"
              @update:model-value="styleTouched = true"
            />
          </div>
        </div>

        <div class="sp__actions">
          <button type="button" class="btn btn--primary" :disabled="styleBusy" @click="saveStyle(false)">
            {{ styleBusy ? t('saving') : t('panelStyleSave') }}
          </button>
          <button type="button" class="btn" :disabled="styleBusy" @click="saveStyle(true)">
            {{ t('resynth') }}
          </button>
          <span v-if="styleMsg" class="sp__msg" :class="`sp__msg--${styleTone}`" role="status">{{ styleMsg }}</span>
        </div>
      </section>
    </template>
  </Panel>
</template>

<style scoped>
.sp__block {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.sp__rows {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.sp__word-row {
  display: grid;
  grid-template-columns: minmax(6rem, 1fr) minmax(8.5rem, auto) auto auto;
  gap: var(--sp-2);
  align-items: end;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface);
}

.sp__lists,
.sp__acks {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 15rem), 1fr));
  gap: var(--sp-3);
}

.sp__row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-3);
  align-items: end;
}

.sp__filler-controls {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 14rem), 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.sp__scenes {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 14rem), 1fr));
  gap: var(--sp-3);
}

.sp__scene {
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface);
}

.sp__details {
  border-top: 1px solid var(--line);
  padding-top: var(--sp-2);
}

.sp__details > summary {
  cursor: pointer;
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--muted);
}

.sp__tokens {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  margin-top: var(--sp-2);
}

.sp__token-row {
  display: grid;
  grid-template-columns: minmax(5rem, 8rem) 1fr auto;
  gap: var(--sp-2);
  align-items: end;
}

.sp__token-word {
  font-size: var(--fs-sm);
  font-weight: 600;
  padding-bottom: 0.35rem;
}

.sp__compiled {
  max-height: 10rem;
  overflow: auto;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface);
  font-size: var(--fs-xs);
}

.sp__hint {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.sp__warnings {
  margin: 0;
  padding-left: var(--sp-4);
  color: var(--warn);
  font-size: var(--fs-xs);
}

.sp__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.sp__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.sp__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

@media (max-width: 720px) {
  .sp__word-row {
    grid-template-columns: 1fr 1fr;
  }
  .sp__token-row {
    grid-template-columns: 1fr;
  }
}
</style>
