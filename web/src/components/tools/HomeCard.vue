<script setup lang="ts">
/**
 * R1j/P0-b + R2-BM5: the household-light tool's REAL state and its one
 * configuration surface.
 *
 * R1j replaced an "unregistered" placeholder that answered `DeviceConfirmed`
 * out of an in-memory mock (2026-11-12 21:53). R2-BM5 adds the adapter: the
 * card edits the Home Assistant connection, the entity whitelist and the
 * write-only token; the probe really calls the backend (auth + one entity
 * read) and reports how many HTTP requests it made. Saving applies
 * immediately, then probes once — the state word below comes from the same
 * backend the assistant uses.
 */
import { computed, ref, watch } from 'vue'
import {
  discoverHome,
  getHome,
  putHome,
  testHome,
  type DiscoveredHomeEntity,
  type HomeDiscovery,
  type HomeEntity,
  type HomeProbe,
  type HomeStatus,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import Select from '../Select.vue'
import TextInput from '../TextInput.vue'
import NumberField from '../NumberField.vue'
import PasswordField from '../PasswordField.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

interface Draft {
  backend: 'none' | 'home_assistant'
  entity_source: 'api' | 'exposed'
  base_url: string
  entities: HomeEntity[]
  timeout_ms: number
  retries: number
}

const home = useResource((signal) => getHome({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => home.load(), 15000)

const state = computed<HomeStatus | null>(() => home.data.value)
const draft = ref<Draft | null>(null)
const token = ref('')
const clearToken = ref(false)
const probe = ref<HomeProbe | null>(null)
const { busy, message, tone, run } = useSave()

// R2-BM6.1: discovery. The card reads HA's entity catalogue and pre-selects
// the supported entities on the first run; the whitelist below is still the
// only thing `home.set` will accept.
const discovered = ref<DiscoveredHomeEntity[]>([])
const discovery = ref<HomeDiscovery | null>(null)
const discovering = ref(false)
const discoveryError = ref('')
const search = ref('')
const domainFilter = ref('all')
const showUnsupported = ref(false)
const autoTried = ref(false)

const SUPPORTED_DOMAINS = ['light', 'switch', 'cover', 'fan', 'input_boolean']

const domainOptions = computed(() => {
  const counts = discovery.value?.domains ?? {}
  return [
    { value: 'all', label: t('homeAllDomains') },
    ...SUPPORTED_DOMAINS.filter((d) => (counts[d] ?? 0) > 0).map((d) => ({
      value: d,
      label: `${d} (${counts[d]})`,
    })),
  ]
})

const filteredDiscovered = computed(() => {
  let rows = discovered.value
  if (!showUnsupported.value) rows = rows.filter((e) => e.supported)
  if (domainFilter.value !== 'all') rows = rows.filter((e) => e.domain === domainFilter.value)
  const q = search.value.trim().toLowerCase()
  if (q) {
    rows = rows.filter(
      (e) => e.name.toLowerCase().includes(q) || e.entity.toLowerCase().includes(q),
    )
  }
  return rows
})

const selectedIds = computed(
  () => new Set((draft.value?.entities ?? []).map((e) => e.entity)),
)
const discoveredIds = computed(() => new Set(discovered.value.map((e) => e.entity)))

function isSelected(entity: string): boolean {
  return selectedIds.value.has(entity)
}

function isManual(entity: string): boolean {
  return discovered.value.length > 0 && !discoveredIds.value.has(entity)
}

function toggleDiscovered(e: DiscoveredHomeEntity): void {
  if (!draft.value || !e.supported) return
  const at = draft.value.entities.findIndex((x) => x.entity === e.entity)
  if (at >= 0) draft.value.entities.splice(at, 1)
  else draft.value.entities.push({ name: e.name || e.entity, entity: e.entity })
}

async function runDiscovery(auto = false): Promise<void> {
  if (!draft.value || draft.value.backend !== 'home_assistant') return
  discovering.value = true
  discoveryError.value = ''
  try {
    const result = await discoverHome()
    discovery.value = result
    if (!result.ok) {
      if (!auto) {
        discoveryError.value = [result.code, result.detail].filter(Boolean).join(' · ')
      }
      return
    }
    discovered.value = result.entities
    // First discovery: start from what the house actually has. The user then
    // filters/removes; an existing hand-edited whitelist is never clobbered.
    if (draft.value.entities.length === 0) {
      draft.value.entities = result.entities
        .filter((e) => e.supported)
        .slice(0, 64)
        .map((e) => ({ name: e.name || e.entity, entity: e.entity }))
    }
  } catch (err) {
    if (!auto) discoveryError.value = errorDetail(err)
  } finally {
    discovering.value = false
  }
}

watch(
  () => home.data.value,
  (s) => {
    if (s && draft.value === null) draft.value = fromStatus(s)
  },
  { immediate: true },
)

function fromStatus(s: HomeStatus): Draft {
  return {
    backend: s.backend,
    entity_source: s.entity_source === 'exposed' ? 'exposed' : 'api',
    base_url: s.base_url ?? '',
    entities: (s.entities ?? []).map((e) => ({ ...e })),
    timeout_ms: s.timeout_ms ?? 4000,
    retries: s.retries ?? 1,
  }
}

const backendOptions = computed(() => [
  { value: 'none', label: t('homeBackendNone') },
  { value: 'home_assistant', label: t('homeBackendHa') },
])

// R2-BM6.3: the two connection modes are not interchangeable, so switching
// one drops the old directory instead of showing a half-valid list.
const sourceOptions = computed(() => [
  { value: 'api', label: t('homeSourceApi') },
  { value: 'exposed', label: t('homeSourceExposed') },
])

function onSource(value: string): void {
  if (!draft.value) return
  const next = value === 'exposed' ? 'exposed' : 'api'
  if (next === draft.value.entity_source) return
  draft.value.entity_source = next
  draft.value.entities = []
  discovered.value = []
  discovery.value = null
  discoveryError.value = ''
  probe.value = null
}

const toneOf = computed(() => {
  if (probe.value) return probe.value.ok ? 'ok' : 'warn'
  return state.value?.state === 'ready' ? 'ok' : 'warn'
})
const stateWord = computed(() => {
  const s = state.value
  if (!s) return ''
  if (s.state === 'unconfigured') return t('homeStateUnconfigured')
  if (s.state === 'ready') return t('weatherStateReady')
  if (s.state === 'disabled') return t('weatherStateDisabled')
  return s.state
})
const backendLabel = computed(() => {
  const s = state.value
  if (!s) return ''
  return s.backend === 'home_assistant' ? t('homeBackendHa') : t('homeBackendNone')
})
const probeTone = computed(() => (probe.value?.ok ? 'ok' : 'danger'))
const probeWord = computed(() => {
  if (!probe.value) return ''
  return probe.value.ok ? t('homeProbeOk') : t('homeProbeFailed')
})

function onBackend(value: string): void {
  if (!draft.value) return
  draft.value.backend = value === 'home_assistant' ? 'home_assistant' : 'none'
  if (draft.value.backend === 'none') {
    discovered.value = []
    discovery.value = null
    discoveryError.value = ''
  }
}

// First visit with a complete CONNECTION: discover without making the user
// click. An empty whitelist is exactly the case discovery is for, so the
// trigger cannot require `state === ready`.
const connectionReady = computed(() => {
  const d = draft.value
  return (
    d?.backend === 'home_assistant' &&
    !!d.base_url.trim() &&
    (state.value?.token_set ?? false)
  )
})
watch(
  connectionReady,
  (ok) => {
    if (ok && !autoTried.value) {
      autoTried.value = true
      void runDiscovery(true)
    }
  },
  { immediate: true },
)

function addEntity(): void {
  draft.value?.entities.push({ name: '', entity: '' })
}

function removeEntity(i: number): void {
  draft.value?.entities.splice(i, 1)
}

async function save(): Promise<void> {
  const d = draft.value
  if (!d) return
  await run(async () => {
    const wasConfigured = !!home.data.value && home.data.value.availability !== 'unconfigured'
    const body: Parameters<typeof putHome>[0] = {
      backend: d.backend,
      entity_source: d.entity_source,
      base_url: d.base_url,
      entities: d.entities,
      timeout_ms: d.timeout_ms,
      retries: d.retries,
    }
    if (clearToken.value) body.token = ''
    else if (token.value.trim() !== '') body.token = token.value
    let saved = await putHome(body)
    draft.value = fromStatus(saved)
    token.value = ''
    clearToken.value = false
    // The save path below owns discovery; do not let the connection watcher
    // fire a second /api/states request.
    autoTried.value = true
    // First-run path ONLY: saving the connection with an empty whitelist
    // discovers the house and persists the pre-selected entities, so the user
    // starts from what HA has instead of typing entity ids. It must not run
    // again for an established installation — clearing every entity and
    // saving used to bring them all back (audit 2026-11-14, P1).
    const firstRun = !wasConfigured
    if (
      firstRun &&
      saved.backend === 'home_assistant' &&
      saved.entities.length === 0 &&
      saved.base_url.trim()
    ) {
      await runDiscovery(true)
      if (draft.value.entities.length) {
        saved = await putHome({
          backend: 'home_assistant',
          base_url: draft.value.base_url,
          entities: draft.value.entities,
          timeout_ms: draft.value.timeout_ms,
          retries: draft.value.retries,
        })
        draft.value = fromStatus(saved)
      }
    }
    await home.load()
    // The user's rule: save → probe → read back. A failed probe is shown in
    // the probe box, not thrown: the configuration itself was saved.
    probe.value = await testHome()
    return t('homeSaved')
  })
}

async function runProbe(): Promise<void> {
  await run(async () => {
    probe.value = await testHome()
    await home.load()
    return probe.value.ok ? t('homeProbeOk') : t('homeProbeFailed')
  })
}
</script>

<template>
  <Panel :title="t('toolHaTitle')" :hint="t('toolHaHint')" icon="home" :tone="toneOf">
    <template #actions>
      <Pill :tone="toneOf" size="sm">{{ stateWord || t('loading') }}</Pill>
    </template>

    <ErrorNotice v-if="home.error.value" :error="home.error.value" @retry="home.load" />
    <!-- Audit R2-26: reading is not empty. -->
    <LoadingBlock v-else-if="!state || !draft" :label="t('loading')" />

    <div v-else class="home">
      <div class="home__summary">
        <span class="home__summary-text">
          {{
            state.backend === 'home_assistant' && state.state === 'ready'
              ? t('homeSummaryConnected', { backend: backendLabel, n: (state.entities ?? []).length })
              : state.backend === 'home_assistant'
                ? (state.claim || t('homeUnconfiguredHint'))
                : t('homeSummaryUnconfigured')
          }}
        </span>
      </div>

      <!-- R2-BM5: concise everyday connection controls; verbose entity lists
           and low-level HA options live inside progressive disclosures below. -->
      <div class="home__form">
        <Select
          :model-value="draft.backend"
          :label="t('homeBackend')"
          :options="backendOptions"
          :hint="t('homeBackendHint')"
          @update:model-value="onBackend"
        />

        <template v-if="draft.backend === 'home_assistant'">
          <div class="home__conn-grid">
            <TextInput v-model="draft.base_url" :label="t('homeAddr')" :hint="t('homeAddrHint')" mono />
            <div>
              <PasswordField
                v-model="token"
                :label="t('homeToken')"
                :is-set="state.token_set"
                :hint="t('homeTokenHint')"
              />
              <div v-if="state.token_set" class="home__clear">
                <Toggle v-model="clearToken" :label="t('homeClearToken')" :hint="t('homeClearTokenHint')" />
              </div>
            </div>
          </div>

          <details class="home__bounds" :open="draft.entities.length === 0 && connectionReady">
            <summary>{{ t('homeManageDevices', { n: draft.entities.length }) }}</summary>
            <div class="home__disclosure-body">
              <!-- Mode B: the whole house, filtered and edited here. -->
              <div v-if="draft.entity_source === 'api'" class="home__entities">
                <div class="home__section-head">
                  <p class="home__label">{{ t('homeEntities') }}</p>
                  <Pill size="sm" mono>{{ draft.entities.length }}</Pill>
                </div>
                <p class="home__hint">{{ t('homeEntityHint') }}</p>
                <p v-if="!draft.entities.length" class="home__hint">{{ t('homeNoEntities') }}</p>
                <div v-for="(entity, i) in draft.entities" :key="i" class="home__entity">
                  <TextInput v-model="entity.name" :label="t('homeEntityName')" />
                  <TextInput v-model="entity.entity" :label="t('homeEntityId')" mono />
                  <Pill v-if="isManual(entity.entity)" size="sm" tone="info">{{ t('homeManualEntity') }}</Pill>
                  <button type="button" class="btn btn--danger" :disabled="busy" @click="removeEntity(i)">
                    {{ t('homeRemoveEntity') }}
                  </button>
                </div>
                <button type="button" class="btn" :disabled="busy" @click="addEntity">
                  {{ t('homeAddEntity') }}
                </button>
              </div>

              <!-- Mode A: HA's Assist exposure decides; this list is read-only. -->
              <div v-else class="home__entities">
                <div class="home__section-head">
                  <p class="home__label">{{ t('homeEntities') }}</p>
                  <Pill size="sm" mono>{{ draft.entities.length }}</Pill>
                </div>
                <p class="home__hint">{{ t('homeExposedReadOnly') }}</p>
                <p v-if="!draft.entities.length" class="home__hint">{{ t('homeNoEntities') }}</p>
                <div
                  v-for="(entity, i) in draft.entities"
                  :key="i"
                  class="home__entity home__entity--readonly"
                >
                  <span class="home__found-name">{{ entity.name }}</span>
                  <span class="mono home__found-id">{{ entity.entity }}</span>
                </div>
              </div>

              <div class="home__discover">
                <div class="home__section-head">
                  <p class="home__label">{{ t('homeDiscover') }}</p>
                  <button type="button" class="btn" :disabled="discovering" @click="runDiscovery(false)">
                    {{ discovering ? t('homeDiscovering') : t('homeRediscover') }}
                  </button>
                </div>
                <p class="home__hint">
                  {{ draft.entity_source === 'exposed' ? t('homeExposedHint') : t('homeDiscoverHint') }}
                </p>
                <p class="home__hint home__hint--warn">{{ t('homeDiscoverUsesSaved') }}</p>

                <div class="home__filters">
                  <input
                    v-model="search"
                    class="home__search"
                    :placeholder="t('homeSearchEntities')"
                  />
                  <Select v-model="domainFilter" :label="t('homeDomain')" :options="domainOptions" />
                  <label class="home__check">
                    <input v-model="showUnsupported" type="checkbox" />
                    <span>{{ t('homeShowUnsupported') }}</span>
                  </label>
                </div>

                <p v-if="discovery?.ok" class="home__hint">
                  {{
                    t('homeDiscovered', {
                      n: discovery.entities.length,
                      supported: discovery.supported ?? 0,
                      requests: discovery.http_requests,
                      ms: discovery.latency_ms,
                    })
                  }}
                </p>

                <ul v-if="filteredDiscovered.length" class="home__discovered">
                  <li v-for="item in filteredDiscovered" :key="item.entity" class="home__found">
                    <label v-if="draft.entity_source === 'api'" class="home__check">
                      <input
                        type="checkbox"
                        :checked="isSelected(item.entity)"
                        :disabled="!item.supported"
                        @change="toggleDiscovered(item)"
                      />
                      <span class="home__found-name">{{ item.name }}</span>
                    </label>
                    <span v-else class="home__found-name">{{ item.name }}</span>
                    <span class="mono home__found-id">{{ item.entity }}</span>
                    <span class="mono home__found-state">{{ item.state }}</span>
                    <Pill v-if="!item.supported" size="sm" tone="warn">{{ t('homeUnsupported') }}</Pill>
                  </li>
                </ul>
                <p v-else-if="discovery?.ok" class="home__hint">{{ t('homeDiscoverEmpty') }}</p>
                <p v-if="!discovery?.ok && !connectionReady" class="home__hint">
                  {{ t('homeDiscoveryUnconfigured') }}
                </p>
                <p v-if="discoveryError" class="home__msg home__msg--danger">
                  <template v-if="draft.entity_source === 'exposed'">{{ t('homeExposedFailed') }} </template>{{ discoveryError }}
                </p>
              </div>
            </div>
          </details>

          <details class="home__bounds">
            <summary>{{ t('homeTechDetails') }}</summary>
            <div class="home__disclosure-body">
              <Select
                :model-value="draft.entity_source"
                :label="t('homeSource')"
                :options="sourceOptions"
                :hint="t('homeSourceHint')"
                @update:model-value="onSource"
              />
              <div class="home__bounds-grid">
                <NumberField
                  v-model="draft.timeout_ms"
                  :label="t('homeTimeout')"
                  :min="500"
                  :max="15000"
                  unit="ms"
                />
                <NumberField v-model="draft.retries" :label="t('homeRetries')" :min="0" :max="3" />
              </div>
              <dl class="home__facts">
                <div class="home__fact">
                  <dt>{{ t('homeTools') }}</dt>
                  <dd class="mono">{{ (state.tools ?? []).join(', ') }} · {{ state.state }}</dd>
                </div>
                <div class="home__fact">
                  <dt>{{ t('homeClaim') }}</dt>
                  <dd>{{ state.claim }}</dd>
                </div>
              </dl>
              <p v-if="state.detail" class="home__hint">{{ state.detail }}</p>
              <p class="home__note">{{ t('homeReadbackNote') }}</p>
            </div>
          </details>
        </template>

        <p v-else class="home__note">{{ t('homeRefused') }}</p>

        <div class="home__actions">
          <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
            {{ busy ? t('saving') : t('homeSave') }}
          </button>
          <button type="button" class="btn" :disabled="busy" @click="runProbe">
            {{ busy ? t('homeProbing') : t('homeProbe') }}
          </button>
          <span v-if="message" class="home__msg" :class="`home__msg--${tone}`">{{ message }}</span>
        </div>
      </div>

      <div v-if="probe" class="home__probe">
        <div class="home__probe-head">
          <Pill :tone="probeTone" size="sm">{{ probeWord }}</Pill>
          <span class="mono">{{ probe.state }}<template v-if="probe.code"> · {{ probe.code }}</template></span>
        </div>
        <p v-if="probe.detail" class="home__hint">{{ probe.detail }}</p>
        <dl class="home__probe-facts">
          <div v-if="probe.entity?.name">
            <dt>{{ t('homeProbeEntityState') }}</dt>
            <dd class="mono">{{ probe.entity.name }} · {{ probe.entity.state }}</dd>
          </div>
          <div>
            <dt>{{ t('homeProbeRequests') }}</dt>
            <dd class="mono">{{ probe.http_requests }}</dd>
          </div>
          <div>
            <dt>{{ t('homeProbeLatency') }}</dt>
            <dd class="mono">{{ probe.latency_ms }} ms</dd>
          </div>
        </dl>
      </div>
    </div>
  </Panel>
</template>

<style scoped>
.home {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.home__summary {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  font-size: var(--fs-sm);
}

.home__conn-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.home__disclosure-body {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  margin-top: var(--sp-2);
}

.home__facts {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-2);
  margin: 0;
}

.home__fact {
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.home__fact dt {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.home__fact dd {
  margin: var(--sp-1) 0 0;
  font-size: var(--fs-sm);
}

.home__detail {
  font-size: var(--fs-sm);
}

.home__form {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding-top: var(--sp-2);
  border-top: 1px solid var(--line);
}

.home__entities {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.home__entity {
  display: grid;
  grid-template-columns: minmax(8rem, 1fr) minmax(10rem, 1.4fr) auto auto;
  gap: var(--sp-2);
  align-items: end;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

/* Mode A rows are read-only: name + id, no controls. */
.home__entity--readonly {
  grid-template-columns: minmax(8rem, 1fr) minmax(10rem, 1.4fr);
  align-items: center;
}

.home__section-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-2);
}

.home__label {
  font-size: var(--fs-sm);
  font-weight: 600;
}

.home__discover {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding-top: var(--sp-2);
  border-top: 1px dashed var(--line);
}

.home__filters {
  display: grid;
  grid-template-columns: minmax(12rem, 1.5fr) minmax(9rem, 1fr) auto;
  gap: var(--sp-2);
  align-items: end;
}

.home__search {
  width: 100%;
  padding: 0.45rem 0.6rem;
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface);
  color: var(--ink);
  font: inherit;
}

.home__check {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-sm);
  color: var(--ink-2);
}

.home__discovered {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  max-height: 22rem;
  overflow: auto;
  margin: 0;
  padding: 0;
  list-style: none;
}

.home__found {
  display: grid;
  grid-template-columns: minmax(10rem, 1fr) minmax(10rem, 1.2fr) auto auto;
  gap: var(--sp-2);
  align-items: center;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  font-size: var(--fs-sm);
}

.home__found-name {
  font-weight: 600;
}

.home__found-id,
.home__found-state {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.home__msg--danger {
  color: var(--danger);
}

.home__hint {
  margin: 0;
  color: var(--muted);
  font-size: var(--fs-xs);
}

.home__note {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.home__bounds summary {
  cursor: pointer;
  color: var(--muted);
  font-size: var(--fs-xs);
}

.home__bounds summary:hover {
  color: var(--ink);
}

.home__bounds-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(10rem, 1fr));
  gap: var(--sp-2);
  margin-top: var(--sp-2);
}

.home__clear {
  margin-top: var(--sp-2);
}

.home__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
  /* Audit R2-16: save and probe stay reachable while the entity list scrolls. */
  position: sticky;
  bottom: 0;
  z-index: 1;
  background: var(--surface);
  padding: var(--sp-2) 0;
  border-top: 1px solid var(--line);
}

.home__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.home__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.home__probe {
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.home__probe-head {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-sm);
}

.home__probe-facts {
  display: flex;
  gap: var(--sp-4);
  flex-wrap: wrap;
  margin: var(--sp-2) 0 0;
}

.home__probe-facts dt {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.home__probe-facts dd {
  margin: 0;
  font-size: var(--fs-sm);
}

@media (max-width: 900px) {
  .home__entity {
    grid-template-columns: 1fr;
  }
}

@media (max-width: 640px) {
  .home__filters,
  .home__found {
    grid-template-columns: 1fr;
  }
}
</style>
