<script setup lang="ts">
/**
 * Web search (M2.5): a registered service, not a promise.
 *
 * One card configures the endpoint, the result count and the write-only key.
 * The assistant can only say 「查到的」 once this service is ready; an
 * unconfigured card is a real state (the tool refuses with
 * SEARCH_UNCONFIGURED and sends ZERO requests). The probe is one REAL query,
 * which is also the only honest way to prove the key works.
 */
import { computed, ref, watch } from 'vue'
import { getSearch, putSearch, testSearch, type SearchProbe, type SearchStatus } from '../../api'
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
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'

const search = useResource((signal) => getSearch({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => search.load(), 15000)

interface Draft {
  enabled: boolean
  baseUrl: string
  count: number
  timeoutMs: number
  apiKey: string
  revision: string
}

const draft = ref<Draft | null>(null)
const probe = ref<SearchProbe | null>(null)
const probing = ref(false)
const probeError = ref('')
const { busy, message, tone, run } = useSave()

function fromStatus(status: SearchStatus): Draft {
  return {
    enabled: status.enabled,
    baseUrl: status.base_url ?? '',
    count: status.count ?? 6,
    timeoutMs: status.timeout_ms ?? 8000,
    apiKey: '',
    revision: status.revision,
  }
}

watch(
  () => search.data.value,
  (status) => {
    if (status && draft.value === null) draft.value = fromStatus(status)
  },
  { immediate: true },
)

const stateLabel = computed(() => {
  switch (search.data.value?.state) {
    case 'ready':
      return t('searchStateReady')
    case 'disabled':
      return t('searchStateDisabled')
    default:
      return t('searchStateUnconfigured')
  }
})

const stateTone = computed(() => {
  switch (search.data.value?.state) {
    case 'ready':
      return 'ok' as const
    case 'disabled':
      return 'warn' as const
    default:
      return 'info' as const
  }
})

const maxResults = computed(() => search.data.value?.max_results ?? 8)

async function save(): Promise<void> {
  if (!draft.value) return
  const current = draft.value
  await run(async () => {
    const status = await putSearch(
      {
        enabled: current.enabled,
        base_url: current.baseUrl.trim(),
        count: Math.max(1, Math.min(50, Math.round(current.count) || 6)),
        timeout_ms: current.timeoutMs,
        ...(current.apiKey ? { api_key: current.apiKey } : {}),
        revision: current.revision || undefined,
      },
      { timeoutMs: 10000 },
    )
    // Adopt the server's normalized block; the key box is always cleared so a
    // stale value can never be re-submitted by accident.
    draft.value = fromStatus(status)
    await search.load()
  })
}

async function clearKey(): Promise<void> {
  if (!draft.value) return
  await run(async () => {
    const status = await putSearch(
      { api_key: '', revision: draft.value?.revision || undefined },
      { timeoutMs: 10000 },
    )
    draft.value = fromStatus(status)
    await search.load()
  })
}

async function runProbe(): Promise<void> {
  probing.value = true
  probe.value = null
  probeError.value = ''
  try {
    probe.value = await testSearch()
  } catch (err) {
    probeError.value = errorDetail(err)
  } finally {
    probing.value = false
  }
}

function reload(): void {
  void search.load().then(() => {
    if (search.data.value) draft.value = fromStatus(search.data.value)
  })
  probe.value = null
  probeError.value = ''
}
</script>

<template>
  <Panel
    icon="search"
    apply="next-turn"
    :title="t('searchTitle')"
    :hint="t('searchHint')"
    :tone="search.data.value?.state === 'ready' ? 'default' : 'warn'"
  >
    <template #actions>
      <Pill :tone="stateTone" size="sm">{{ stateLabel }}</Pill>
      <Pill v-if="search.data.value" size="sm" mono>
        {{ t('searchCountPill', { n: search.data.value.count }) }}
      </Pill>
    </template>

    <ErrorNotice v-if="search.error.value" :error="search.error.value" compact @retry="search.load()" />

    <template v-if="draft">
      <Toggle v-model="draft.enabled" :label="t('searchEnabled')" />

      <TextInput
        v-model="draft.baseUrl"
        :label="t('searchBaseUrl')"
        placeholder="https://api.deepseek.com/v1/tools/search"
      />

      <PasswordField
        v-model="draft.apiKey"
        :label="t('searchApiKey')"
        :is-set="search.data.value?.api_key_set ?? false"
        :placeholder="t('searchApiKeyPlaceholder')"
      />

      <div class="search__grid">
        <NumberField
          v-model="draft.count"
          :label="t('searchCount')"
          :min="1"
          :max="50"
          :hint="t('searchCountHint', { max: maxResults })"
        />
        <NumberField v-model="draft.timeoutMs" :label="t('searchTimeout')" :min="500" :max="15000" />
      </div>

      <div class="search__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
        </button>
        <button type="button" class="btn" :disabled="probing" @click="runProbe">
          <AppIcon name="probes" :size="13" /> {{ probing ? t('testing') : t('searchTest') }}
        </button>
        <button
          v-if="search.data.value?.api_key_set"
          type="button"
          class="btn btn--danger"
          :disabled="busy"
          @click="clearKey"
        >
          {{ t('searchApiKeyClear') }}
        </button>
        <button type="button" class="btn" @click="reload">
          <AppIcon name="reload" :size="13" /> {{ t('reload') }}
        </button>
        <span v-if="message" class="search__msg" :class="`search__msg--${tone}`">{{ message }}</span>
      </div>

      <p v-if="search.data.value?.detail" class="search__detail">{{ search.data.value.detail }}</p>

      <div v-if="probe" class="search__probe">
        <Pill :tone="probe.ok ? 'ok' : 'danger'" size="sm" mono>
          {{
            probe.ok
              ? t('searchProbeOk', {
                  n: probe.count,
                  requests: probe.http_requests,
                  ms: probe.latency_ms,
                })
              : t('searchProbeFail', { detail: probe.detail || probe.code || probe.state })
          }}
        </Pill>
        <template v-if="probe.samples?.length">
          <p class="search__subtitle">{{ t('searchProbeSamples') }}</p>
          <ul class="search__samples">
            <li v-for="(s, i) in probe.samples" :key="`s-${i}`">
              <span class="search__sample-title">{{ s.title }}</span>
              <span class="search__msg mono">{{ s.date || t('searchProbeNoDate') }}</span>
              <a v-if="s.url" class="search__msg mono" :href="s.url" target="_blank" rel="noreferrer noopener">
                {{ s.url }}
              </a>
            </li>
          </ul>
        </template>
      </div>
      <p v-if="probeError" class="search__msg search__msg--error">{{ probeError }}</p>
      <p class="search__msg">{{ t('searchZeroRequests') }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.search__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.search__actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
}

.search__detail {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.search__subtitle {
  font-size: var(--fs-sm);
  font-weight: 650;
  color: var(--ink-2);
}

.search__msg {
  font-size: var(--fs-sm);
  color: var(--muted);
  overflow-wrap: anywhere;
}

.search__msg--error {
  color: var(--danger);
}

.search__msg--ok {
  color: var(--ok);
}

.search__probe {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.search__samples {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  margin: 0;
  padding: 0;
  list-style: none;
  font-size: var(--fs-sm);
}

.search__samples li {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--sp-2);
}

.search__sample-title {
  font-weight: 600;
}
</style>
