<script setup lang="ts">
/**
 * Unified voice interrupt & quick-command vocabulary card.
 *
 * Combines the on-device quick command table (reflex) and the spoken
 * stop/continue phrase lists (stop_intents) into one plain-language panel,
 * while keeping low-frequency device KWS deploy details (token spellings,
 * intents_hash, local_exit receipt, compiled kw.txt) inside a collapsed
 * <details> block.
 */
import { computed, ref, watch } from 'vue'
import {
  deployReflex,
  getReflex,
  getReflexTokens,
  putMode,
  putReflex,
  type ModeDevice,
  type ModeSnapshot,
  type ReflexWord,
} from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import TextInput from '../TextInput.vue'
import Select from '../Select.vue'
import PhraseList from '../PhraseList.vue'
import StatTile from '../StatTile.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const reflex = useResource((signal) => getReflex({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => reflex.load(), 15000)

const draft = ref<ReflexWord[] | null>(null)
const tokenError = ref('')
const { busy, message, tone, run } = useSave()
const {
  busy: stopBusy,
  message: stopMessage,
  tone: stopTone,
  run: runStop,
} = useSave()

const keywordEnabled = ref<boolean | null>(null)
const keywordDirty = ref(false)

interface StopDraft {
  enabled: boolean
  stop: string[]
  continue: string[]
}

const stopDraft = ref<StopDraft | null>(null)

watch(
  () => props.mode,
  (mode) => {
    if (mode && !keywordDirty.value) {
      keywordEnabled.value = mode.keyword_interrupt !== false
    }
    if (mode?.stop_intents && stopDraft.value === null) {
      stopDraft.value = {
        enabled: mode.stop_intents.enabled !== false,
        stop: [...mode.stop_intents.stop_words],
        continue: [...mode.stop_intents.continue_words],
      }
    }
  },
  { immediate: true },
)

watch(
  () => reflex.data.value,
  (state) => {
    if (state && draft.value === null) {
      draft.value = state.words.map((w) => ({ ...w }))
      autoTokens.value = new Set(state.words.map((_, i) => i))
    }
  },
  { immediate: true },
)

function onKeywordToggle(value: boolean): void {
  keywordEnabled.value = value
  keywordDirty.value = true
}

async function saveKeyword(): Promise<void> {
  if (!props.device || keywordEnabled.value === null) return
  const ok = await run(async () => {
    await putMode(props.device!.device, { keyword_interrupt: keywordEnabled.value! })
    await props.refresh()
    return t('kwsSaved')
  })
  if (ok) keywordDirty.value = false
}

async function saveStop(): Promise<void> {
  if (!props.device || !stopDraft.value) return
  const current = stopDraft.value
  await runStop(async () => {
    await putMode(props.device!.device, {
      stop_intents: {
        enabled: current.enabled,
        stop_words: current.stop,
        continue_words: current.continue,
      },
    })
    await props.refresh()
  })
}

function spotterLabel(state: string | null | undefined): string {
  switch (state) {
    case 'running':
      return t('kwsRunning')
    case 'starting':
      return t('kwsStarting')
    case 'off':
      return t('kwsOff')
    case 'down':
      return t('kwsDown')
    default:
      return t('kwsUnknown')
  }
}

function spotterTone(state: string | null | undefined): 'ok' | 'warn' | 'danger' | 'muted' {
  if (state === 'running') return 'ok'
  if (state === 'starting') return 'warn'
  if (state === 'down') return 'danger'
  return 'muted'
}

const intentOptions = computed(() => [
  { value: 'wake', label: t('reflexIntentWake') },
  { value: 'retreat', label: t('reflexIntentRetreat') },
  { value: 'pause', label: t('reflexIntentPause') },
  { value: 'continue', label: t('reflexIntentContinue') },
  { value: 'next', label: t('reflexIntentNext') },
  { value: 'stop', label: t('reflexIntentStop') },
])

const deploy = computed(() => props.device?.reflex?.deploy ?? null)
const kws = computed(() => props.device?.kws ?? null)
const compiled = computed(() => reflex.data.value?.compiled ?? '')
const warnings = computed(() => reflex.data.value?.warnings ?? [])
const liveWords = computed(() => props.device?.reflex?.words ?? [])

const localExit = computed(() => {
  const words = liveWords.value.filter((w) => w.enabled && w.intent === 'retreat')
  const armed = props.device?.kws?.local_exit ?? 0
  if (!words.length && armed === 0) return null
  return { armed: armed > 0, armed_count: armed }
})

function counters(text: string): { hits: number; ignored: number; text: number } {
  const w = liveWords.value.find((x) => x.text === text)
  return { hits: w?.hits ?? 0, ignored: w?.ignored ?? 0, text: w?.text_hits ?? 0 }
}

function pathOf(text: string): 'reflex' | 'text' | 'both' | 'untested' {
  const c = counters(text)
  if (c.hits > 0 && c.text > 0) return 'both'
  if (c.hits > 0) return 'reflex'
  if (c.text > 0) return 'text'
  return 'untested'
}

function pathLabel(text: string): string {
  switch (pathOf(text)) {
    case 'reflex':
      return t('reflexPathReflex')
    case 'text':
      return t('reflexPathText')
    case 'both':
      return t('reflexPathBoth')
    default:
      return t('reflexPathUntested')
  }
}

function pathTone(text: string): 'ok' | 'warn' | 'muted' {
  switch (pathOf(text)) {
    case 'reflex':
      return 'ok'
    case 'text':
      return 'warn'
    default:
      return 'muted'
  }
}

function deployTone(state: string | undefined): 'ok' | 'warn' | 'danger' | 'muted' {
  if (state === 'accepted' || state === 'loaded') return 'ok'
  if (state === 'sent') return 'warn'
  if (state === 'rejected') return 'danger'
  return 'muted'
}

function deployStateLabel(state: string | undefined): string {
  switch (state) {
    case 'accepted':
      return t('reflexDeployAccepted')
    case 'loaded':
      return t('reflexDeployLoaded')
    case 'sent':
      return t('reflexDeploySent')
    case 'rejected':
      return t('reflexDeployRejected')
    default:
      return state || '—'
  }
}

const autoTokens = ref<Set<number>>(new Set())
let tokenTimer: number | null = null

function generate(i: number): void {
  const word = draft.value?.[i]
  if (!word) return
  const text = word.text.trim()
  if (!text) return
  void getReflexTokens(text)
    .then((res) => {
      const row = draft.value?.[i]
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
  const word = draft.value?.[i]
  if (!word) return
  if (!autoTokens.value.has(i) && word.tokens.trim() !== '') return
  if (tokenTimer !== null) window.clearTimeout(tokenTimer)
  tokenTimer = window.setTimeout(() => generate(i), 300)
}

function onTokensInput(i: number): void {
  autoTokens.value.delete(i)
}

function addWord(): void {
  if (!draft.value) return
  draft.value.push({ text: '', tokens: '', intent: 'pause', enabled: true })
}

function removeWord(i: number): void {
  draft.value?.splice(i, 1)
}

async function persist(withDeploy: boolean): Promise<void> {
  if (!draft.value) return
  const words = draft.value.map((w) => ({ ...w }))
  await run(async () => {
    const saved = await putReflex({ words, deploy: withDeploy, device: props.device?.device })
    draft.value = saved.words.map((w) => ({ ...w }))
    await reflex.load()
    await props.refresh()
    return withDeploy ? t('reflexDeployed') : t('reflexSaved')
  })
}

async function redeploy(): Promise<void> {
  await run(async () => {
    await deployReflex({ device: props.device?.device })
    await props.refresh()
    await reflex.load()
    return t('reflexDeployed')
  })
}
</script>

<template>
  <Panel icon="kws" :title="t('interruptTitle')" :hint="t('interruptHint')">
    <template #actions>
      <Pill v-if="device?.kws" :tone="spotterTone(device.kws.state)" size="sm">
        {{ t('spotter') }}: {{ spotterLabel(device.kws.state) }}
      </Pill>
      <Pill v-if="deploy" :tone="deployTone(deploy.state)" size="sm">
        {{ t('reflexDeployState') }}: {{ deployStateLabel(deploy.state) }}
      </Pill>
    </template>

    <ErrorNotice v-if="reflex.error.value" :error="reflex.error.value" @retry="reflex.load" />
    <LoadingBlock v-else-if="!draft" :label="t('loading')" />

    <template v-else>
      <div class="iw__section">
        <div class="iw__kws">
          <Toggle
            v-if="keywordEnabled !== null"
            :model-value="keywordEnabled"
            :label="t('kwsEnabled')"
            :hint="t('kwsToggleHint')"
            @update:model-value="onKeywordToggle"
          />
          <button
            type="button"
            class="btn"
            :disabled="busy || keywordEnabled === null || !keywordDirty"
            @click="saveKeyword"
          >
            {{ busy ? t('saving') : t('kwsSave') }}
          </button>
        </div>

        <p class="iw__note">{{ t('reflexWakeHint') }}</p>

        <div class="iw__rows">
          <div v-for="(word, i) in draft" :key="i" class="iw__row">
            <div @input="onWordInput(i)">
              <TextInput v-model="word.text" :label="t('reflexWord')" :maxlength="12" />
            </div>
            <Select v-model="word.intent" :label="t('reflexIntent')" :options="intentOptions" />
            <Toggle
              :model-value="word.enabled"
              :label="t('reflexEnabled')"
              @update:model-value="(v: boolean) => (word.enabled = v)"
            />
            <div
              class="iw__counts mono"
              :title="`${t('reflexHits')} ${counters(word.text).hits} · ${t('reflexTextHits')} ${counters(word.text).text} · ${t('reflexIgnored')} ${counters(word.text).ignored}`"
            >
              <Pill :tone="pathTone(word.text)" size="sm">{{ pathLabel(word.text) }}</Pill>
              <span>{{ counters(word.text).hits }}·{{ counters(word.text).text }}·{{ counters(word.text).ignored }}</span>
            </div>
            <button
              type="button"
              class="btn btn--danger btn--compact"
              :disabled="busy"
              :title="t('reflexRemove')"
              :aria-label="t('reflexRemove')"
              @click="removeWord(i)"
            >
              ×
            </button>
          </div>
        </div>

        <div class="iw__actions">
          <button type="button" class="btn" :disabled="busy" @click="addWord">{{ t('reflexAdd') }}</button>
          <button type="button" class="btn" :disabled="busy" @click="persist(false)">
            {{ busy ? t('saving') : t('reflexSave') }}
          </button>
          <button type="button" class="btn btn--primary" :disabled="busy" @click="persist(true)">
            {{ busy ? t('saving') : t('reflexSaveSync') }}
          </button>
          <span v-if="message" class="iw__msg" :class="`iw__msg--${tone}`">{{ message }}</span>
        </div>

        <ul v-if="warnings.length" class="iw__warnings">
          <li v-for="(w, i) in warnings" :key="i">{{ w }}</li>
        </ul>
        <p v-if="tokenError" class="iw__msg iw__msg--error">{{ tokenError }}</p>
      </div>

      <div v-if="stopDraft" id="stop-words" class="iw__section iw__section--divider">
        <h3 class="iw__subhead">{{ t('stopTitle') }}</h3>
        <p class="iw__note">{{ t('stopHint') }}</p>
        <Toggle v-model="stopDraft.enabled" :label="t('stopEnabled')" />

        <div class="iw__lists">
          <PhraseList
            v-model="stopDraft.stop"
            :label="t('stopWordsTitle')"
            :placeholder="t('phrasePlaceholder')"
            :maxlength="8"
            :disabled="stopBusy"
          />
          <PhraseList
            v-model="stopDraft.continue"
            :label="t('continueWordsTitle')"
            :placeholder="t('phrasePlaceholder')"
            :maxlength="8"
            :disabled="stopBusy"
          />
        </div>

        <p class="iw__hint">{{ t('continueHint') }}</p>

        <div class="iw__actions iw__actions--inline">
          <button type="button" class="btn btn--primary" :disabled="stopBusy" @click="saveStop">
            {{ stopBusy ? t('saving') : t('stopSave') }}
          </button>
          <span v-if="stopMessage" class="iw__msg" :class="`iw__msg--${stopTone}`">{{ stopMessage }}</span>
        </div>
      </div>

      <details class="iw__details">
        <summary>{{ t('reflexDetailsSummary') }}</summary>
        <div class="iw__details-body">
          <div class="iw__pills">
            <Pill v-if="mode?.keyword_phrase" size="sm" mono>
              {{ t('kwsPhrase') }}: {{ mode.keyword_phrase }}
            </Pill>
            <Pill
              v-if="localExit"
              :tone="localExit.armed ? 'ok' : 'warn'"
              size="sm"
              :title="localExit.armed ? t('reflexLocalExitHint') : t('reflexLocalExitMissingHint')"
            >
              {{
                localExit.armed
                  ? t('reflexLocalExitArmed', { n: localExit.armed_count })
                  : t('reflexLocalExitMissing')
              }}
            </Pill>
          </div>

          <div v-if="kws" class="iw__stats">
            <StatTile :label="t('kwsStops')" :value="kws.stops" tone="accent" />
            <StatTile :label="t('kwsWakes')" :value="kws.wakes ?? 0" tone="ok" />
            <StatTile :label="t('kwsIgnored')" :value="kws.ignored" tone="warn" />
            <StatTile :label="t('kwsRestarts')" :value="kws.restarts" tone="muted" />
            <StatTile
              :label="t('kwsLast')"
              :value="kws.last_keyword_ms ? formatTime(kws.last_keyword_ms) : '—'"
              tone="muted"
            />
            <StatTile
              :label="t('kwsWindow')"
              :value="`${kws.detections_since_epoch ?? 0} / ${kws.detections}`"
              tone="muted"
              :note="kws.counted_since_ms ? `${t('kwsWindowSince')} ${formatTime(kws.counted_since_ms)}` : ''"
            />
          </div>

          <p v-if="kws?.state === 'down'" class="iw__warn">{{ t('kwsDownWarn') }}</p>
          <p v-else-if="kws?.state === 'off'" class="iw__hint">{{ t('kwsOffHint') }}</p>
          <p v-if="kws?.detail" class="iw__hint mono">{{ kws.detail }}</p>

          <div class="iw__tokens">
            <div v-for="(word, i) in draft" :key="`tok-${i}`" class="iw__token-row">
              <span class="iw__token-word">{{ word.text || '—' }}</span>
              <div @input="onTokensInput(i)">
                <TextInput v-model="word.tokens" :label="t('reflexTokens')" mono />
              </div>
              <button
                type="button"
                class="btn btn--ghost"
                :disabled="busy || !word.text.trim()"
                :title="t('reflexGenerateHint')"
                @click="generate(i)"
              >
                {{ t('reflexGenerate') }}
              </button>
            </div>
          </div>

          <p class="iw__hint">{{ t('wakeCommandHint') }}</p>
          <p class="iw__hint">{{ t('reflexTokenHint') }}</p>
          <p v-if="localExit" class="iw__hint">
            {{ localExit.armed ? t('reflexLocalExitHint') : t('reflexLocalExitMissingHint') }}
          </p>

          <div v-if="deploy" class="iw__deploy mono">
            <span>{{ t('reflexHash') }}: {{ deploy.hash || '—' }}</span>
            <span>{{ t('reflexWords') }}: {{ deploy.keywords }}</span>
            <span>{{ t('reflexSent') }}: {{ deploy.sent_ms ? formatTime(deploy.sent_ms) : '—' }}</span>
            <span>{{ t('reflexAcked') }}: {{ deploy.acked_ms ? formatTime(deploy.acked_ms) : '—' }}</span>
            <button type="button" class="btn" :disabled="busy" @click="redeploy">
              {{ t('reflexRedeploy') }}
            </button>
          </div>
          <p v-if="deploy?.detail" class="iw__hint mono">{{ deploy.detail }}</p>

          <div class="iw__counters">
            <span v-for="item in mode?.devices ?? []" :key="item.device" class="mono iw__counter">
              {{ item.device }} · {{ t('falseStops') }}: {{ item.stop_intents?.continue_intents ?? 0 }}
            </span>
          </div>

          <details class="iw__file">
            <summary>{{ t('reflexCompiled') }}</summary>
            <pre class="mono">{{ compiled || '—' }}</pre>
            <p class="iw__hint mono">{{ t('reflexHash') }}: {{ reflex.data.value?.hash || '—' }}</p>
          </details>
        </div>
      </details>
    </template>
  </Panel>
</template>

<style scoped>
.iw__section {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.iw__section--divider {
  padding-top: var(--sp-3);
  border-top: 1px solid var(--line);
}

.iw__subhead {
  font-size: var(--fs-base);
  font-weight: 600;
  margin: 0;
}

.iw__kws {
  display: flex;
  align-items: flex-end;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.iw__note {
  color: var(--muted);
  font-size: var(--fs-sm);
}

.iw__rows {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.iw__row {
  display: grid;
  grid-template-columns: minmax(6rem, 1fr) minmax(8.5rem, auto) auto auto auto;
  gap: var(--sp-2);
  align-items: end;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.iw__counts {
  display: flex;
  align-items: center;
  gap: var(--sp-1);
  font-size: var(--fs-xs);
  color: var(--muted);
  white-space: nowrap;
}

.iw__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
  position: sticky;
  bottom: 0;
  z-index: 1;
  background: var(--surface);
  padding: var(--sp-2) 0;
  border-top: 1px solid var(--line);
}

.iw__actions--inline {
  position: static;
  border-top: none;
  padding: 0;
}

.iw__lists {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-4);
}

.iw__details {
  border-top: 1px solid var(--line);
  padding-top: var(--sp-3);
}

.iw__details > summary {
  cursor: pointer;
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--muted);
}

.iw__details-body {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  margin-top: var(--sp-3);
}

.iw__pills {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.iw__stats {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(8.5rem, 1fr));
  gap: var(--sp-2);
}

.iw__tokens {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.iw__token-row {
  display: grid;
  grid-template-columns: minmax(5rem, 8rem) 1fr auto;
  gap: var(--sp-2);
  align-items: end;
}

.iw__token-word {
  font-size: var(--fs-sm);
  font-weight: 600;
  padding-bottom: 0.35rem;
}

.iw__deploy {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
  font-size: var(--fs-xs);
  color: var(--muted);
}

.iw__counters {
  display: flex;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.iw__counter {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.iw__file pre {
  max-height: 12rem;
  overflow: auto;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  font-size: var(--fs-xs);
}

.iw__hint {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.iw__warn {
  color: var(--warn);
  font-size: var(--fs-sm);
}

.iw__warnings {
  margin: 0;
  padding-left: var(--sp-4);
  color: var(--warn);
  font-size: var(--fs-xs);
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

.btn--danger {
  color: var(--danger);
}

.btn--ghost {
  color: var(--muted);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.iw__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.iw__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

@media (max-width: 900px) {
  .iw__row {
    grid-template-columns: 1fr 1fr;
  }
  .iw__token-row {
    grid-template-columns: 1fr;
  }
}
</style>
