<script setup lang="ts">
/**
 * News (R2-BM6): a registered service, not a promise. One card configures the
 * feeds; the assistant reads HEADLINES from them and says so honestly when
 * there is none. There is no sample feed: an unconfigured card is a real
 * state, not a placeholder.
 */
import { computed, ref, watch } from 'vue'
import { getNews, putNews, testNews, type NewsKind, type NewsProbe, type NewsSource, type NewsStatus } from '../../api'
import { errorDetail } from '../../format'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import ErrorNotice from '../ErrorNotice.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import NumberField from '../NumberField.vue'
import Select from '../Select.vue'

const news = useResource((signal) => getNews({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => news.load(), 15000)

interface Draft {
  enabled: boolean
  sources: NewsSource[]
  defaultAlias: string
  timeoutMs: number
  maxSources: number
  revision: string
}

const draft = ref<Draft | null>(null)
const probe = ref<NewsProbe | null>(null)
const probing = ref(false)
const probeError = ref('')
const { busy, message, tone, run } = useSave()

const KIND_OPTIONS: { value: NewsKind; label: string }[] = [
  { value: 'rss', label: 'RSS 2.0' },
  { value: 'atom', label: 'Atom' },
  { value: 'json', label: 'JSON' },
]

function fromStatus(status: NewsStatus): Draft {
  return {
    enabled: status.enabled,
    sources: (status.sources ?? []).map((s) => ({ ...s })),
    defaultAlias: status.default_alias ?? '',
    timeoutMs: status.timeout_ms ?? 5000,
    maxSources: status.max_sources ?? 5,
    revision: status.revision,
  }
}

watch(
  () => news.data.value,
  (status) => {
    if (status && draft.value === null) draft.value = fromStatus(status)
  },
  { immediate: true },
)

const stateLabel = computed(() => {
  switch (news.data.value?.state) {
    case 'ready':
      return t('newsStateReady')
    case 'disabled':
      return t('newsStateDisabled')
    default:
      return t('newsStateUnconfigured')
  }
})

const stateTone = computed(() => {
  switch (news.data.value?.state) {
    case 'ready':
      return 'ok' as const
    case 'disabled':
      return 'warn' as const
    default:
      return 'info' as const
  }
})

const aliasOptions = computed(() => [
  { value: '', label: '—' },
  ...(draft.value?.sources ?? []).map((s) => ({ value: s.alias, label: s.alias })),
])

function addSource(): void {
  if (!draft.value || draft.value.sources.length >= draft.value.maxSources) return
  draft.value.sources.push({
    alias: draft.value.sources.length === 0 ? t('newsAliasPlaceholder') : '',
    url: '',
    kind: 'rss',
    max_items: 5,
  })
}

function removeSource(index: number): void {
  if (!draft.value) return
  const removed = draft.value.sources[index]
  draft.value.sources.splice(index, 1)
  if (removed?.alias === draft.value.defaultAlias) draft.value.defaultAlias = ''
}

async function save(): Promise<void> {
  if (!draft.value) return
  const current = draft.value
  await run(async () => {
    const status = await putNews(
      {
        enabled: current.enabled,
        sources: current.sources.map((s) => ({
          alias: s.alias.trim(),
          url: s.url.trim(),
          kind: s.kind,
          max_items: Math.max(1, Math.min(10, Math.round(s.max_items) || 5)),
        })),
        default_alias: current.defaultAlias,
        timeout_ms: current.timeoutMs,
        max_sources: current.maxSources,
        revision: current.revision || undefined,
      },
      { timeoutMs: 10000 },
    )
    draft.value = fromStatus(status)
    await news.load()
  })
}

async function runProbe(): Promise<void> {
  probing.value = true
  probe.value = null
  probeError.value = ''
  try {
    probe.value = await testNews()
  } catch (err) {
    probeError.value = errorDetail(err)
  } finally {
    probing.value = false
  }
}

function reload(): void {
  void news.load().then(() => {
    if (news.data.value) draft.value = fromStatus(news.data.value)
  })
  probe.value = null
  probeError.value = ''
}
</script>

<template>
  <Panel
    icon="news"
    apply="next-turn"
    :title="t('newsTitle')"
    :hint="t('newsHint')"
    :tone="news.data.value?.state === 'ready' ? 'default' : 'warn'"
  >
    <template #actions>
      <Pill :tone="stateTone" size="sm">{{ stateLabel }}</Pill>
      <Pill v-if="news.data.value" size="sm" mono>
        {{ news.data.value.sources.length }}/{{ news.data.value.max_sources ?? 5 }} {{ t('newsSources') }}
      </Pill>
    </template>

    <ErrorNotice v-if="news.error.value" :error="news.error.value" compact @retry="news.load()" />

    <template v-if="draft">
      <Toggle v-model="draft.enabled" :label="t('newsEnabled')" />

      <div class="news__sources">
        <p class="news__subtitle">{{ t('newsSources') }}</p>
        <p v-if="!draft.sources.length" class="news__empty">{{ t('newsNoSources') }}</p>
        <div v-for="(source, index) in draft.sources" :key="`src-${index}`" class="news__source">
          <label class="wf">
            <span class="wf__label">{{ t('newsAlias') }}</span>
            <input v-model="source.alias" class="wf__input" :placeholder="t('newsAliasPlaceholder')" />
          </label>
          <label class="wf news__url">
            <span class="wf__label">{{ t('newsUrl') }}</span>
            <input v-model="source.url" class="wf__input mono" placeholder="https://example.com/feed.xml" />
          </label>
          <label class="wf">
            <span class="wf__label">{{ t('newsKind') }}</span>
            <select v-model="source.kind" class="wf__input">
              <option v-for="opt in KIND_OPTIONS" :key="opt.value" :value="opt.value">{{ opt.label }}</option>
            </select>
          </label>
          <label class="wf">
            <span class="wf__label">{{ t('newsMaxItems') }}</span>
            <input v-model.number="source.max_items" class="wf__input mono" type="number" min="1" max="10" />
          </label>
          <button type="button" class="btn btn--danger" @click="removeSource(index)">
            {{ t('newsRemoveSource') }}
          </button>
        </div>
        <div>
          <button
            type="button"
            class="btn"
            :disabled="draft.sources.length >= draft.maxSources"
            @click="addSource"
          >
            {{ t('newsAddSource') }}
          </button>
        </div>
      </div>

      <div class="news__grid">
        <Select v-model="draft.defaultAlias" :label="t('newsDefault')" :options="aliasOptions" />
        <NumberField v-model="draft.timeoutMs" :label="t('newsTimeout')" :min="500" :max="15000" />
        <NumberField v-model="draft.maxSources" :label="t('newsMaxSources')" :min="1" :max="50" />
      </div>

      <div class="news__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
        </button>
        <button type="button" class="btn" :disabled="probing" @click="runProbe">
          <AppIcon name="probes" :size="13" /> {{ probing ? t('testing') : t('newsTest') }}
        </button>
        <button type="button" class="btn" @click="reload"><AppIcon name="reload" :size="13" /> {{ t('reload') }}</button>
        <span v-if="message" class="news__msg" :class="`news__msg--${tone}`">{{ message }}</span>
      </div>

      <div v-if="probe" class="news__probe">
        <Pill :tone="probe.ok ? 'ok' : 'danger'" size="sm" mono>
          {{
            probe.ok
              ? t('newsProbeSummary', {
                  ok: probe.sources.filter((s) => s.state === 'ready').length,
                  total: probe.sources.length,
                  requests: probe.http_requests,
                  ms: probe.latency_ms,
                })
              : t('newsProbeFail', { detail: probe.detail || probe.state })
          }}
        </Pill>
        <ul class="news__probe-list">
          <li v-for="item in probe.sources" :key="item.alias">
            <span class="mono">{{ item.alias }}</span>
            <Pill :tone="item.state === 'ready' ? 'ok' : 'danger'" size="sm">
              {{ item.state === 'ready' ? t('newsSourceReady') : t('newsSourceFailed') }}
            </Pill>
            <span v-if="item.state === 'ready'" class="news__msg">{{ t('newsSourceItems', { n: item.items ?? 0, ms: item.latency_ms ?? 0 }) }}</span>
            <span v-else class="news__msg news__msg--error">{{ item.code }} · {{ item.detail }}</span>
          </li>
        </ul>
      </div>
      <p v-if="probeError" class="news__msg news__msg--error">{{ probeError }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.news__sources {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.news__subtitle {
  font-size: var(--fs-sm);
  font-weight: 650;
  color: var(--ink-2);
}

.news__empty {
  color: var(--muted);
  font-size: var(--fs-sm);
}

.news__source {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  gap: var(--sp-2);
  align-items: end;
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.news__url {
  grid-column: span 2;
}

.news__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.wf {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  min-width: 0;
}

.wf__label {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wf__input {
  width: 100%;
  padding: 0.45rem 0.6rem;
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface);
  color: var(--ink);
  font: inherit;
}

.news__actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
}

.news__msg {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.news__msg--error {
  color: var(--danger);
}

.news__msg--ok {
  color: var(--ok);
}

.news__probe {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.news__probe-list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  margin: 0;
  padding: 0;
  list-style: none;
  font-size: var(--fs-sm);
}

.news__probe-list li {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
}
</style>
