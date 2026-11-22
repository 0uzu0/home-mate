<script setup lang="ts">
/**
 * Weather: the first REGISTERED service. One card, no interaction rules —
 * enabling it is all it takes for the assistant to answer weather questions.
 * Places are coordinates; the city search here is the only place that talks
 * to a geocoder.
 */
import { computed, ref, watch } from 'vue'
import {
  geocodePlace,
  getWeather,
  putWeather,
  testWeather,
  type GeocodePlace,
  type WeatherPlace,
  type WeatherProbe,
  type WeatherStatus,
} from '../../api'
import { locale, t } from '../../i18n'
import { errorDetail, formatNumber } from '../../format'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import ErrorNotice from '../ErrorNotice.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'
import Select from '../Select.vue'
import NumberField from '../NumberField.vue'

const weather = useResource((signal) => getWeather({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => weather.load(), 15000)

interface Draft {
  enabled: boolean
  provider: string
  endpoint: string
  apiKeyEnv: string
  apiKey: string
  places: WeatherPlace[]
  defaultAlias: string
  maxDays: number
  revision: string
}

const draft = ref<Draft | null>(null)
const probe = ref<WeatherProbe | null>(null)
const probing = ref(false)
const probeError = ref('')
const query = ref('')
const results = ref<GeocodePlace[]>([])
const searching = ref(false)
const searchError = ref('')
const { busy, message, tone, run } = useSave()

function fromStatus(status: WeatherStatus): Draft {
  return {
    enabled: status.enabled,
    provider: status.provider || 'open_meteo',
    endpoint: status.endpoint ?? '',
    apiKeyEnv: status.api_key_env ?? '',
    apiKey: '',
    places: (status.places ?? []).map((place) => ({
      ...place,
      city_code: place.city_code ?? '',
    })),
    defaultAlias: status.default_alias ?? '',
    maxDays: status.max_days ?? 3,
    revision: status.revision,
  }
}

watch(
  () => weather.data.value,
  (status) => {
    if (status && draft.value === null) draft.value = fromStatus(status)
  },
  { immediate: true },
)

const stateLabel = computed(() => {
  switch (weather.data.value?.state) {
    case 'ready':
      return t('weatherStateReady')
    case 'disabled':
      return t('weatherStateDisabled')
    default:
      return t('weatherStateUnconfigured')
  }
})

const stateTone = computed(() => {
  switch (weather.data.value?.state) {
    case 'ready':
      return 'ok' as const
    case 'disabled':
      return 'warn' as const
    default:
      return 'info' as const
  }
})

const providerOptions = computed(() => [
  { value: 'met_no', label: t('weatherProviderMetNo') },
  { value: 'wttr_in', label: t('weatherProviderWttrIn') },
  { value: 'open_meteo', label: t('weatherProviderOpenMeteo') },
])
const endpointPlaceholder = computed(() => {
  switch (draft.value?.provider) {
    case 'met_no':
      return 'https://api.met.no/weatherapi/locationforecast/2.0/compact'
    case 'wttr_in':
      return 'https://wttr.in'
    default:
      return 'https://api.open-meteo.com/v1/forecast'
  }
})
const aliasOptions = computed(() => [
  { value: '', label: '—' },
  ...(draft.value?.places ?? []).map((place) => ({ value: place.alias, label: place.alias })),
])

function addPlace(): void {
  if (!draft.value) return
  draft.value.places.push({
    alias: draft.value.places.length === 0 ? (locale.value === 'zh' ? '家' : 'home') : '',
    name: '',
    latitude: 0,
    longitude: 0,
    timezone: 'Asia/Shanghai',
    city_code: '',
  })
}

function removePlace(index: number): void {
  if (!draft.value) return
  draft.value.places.splice(index, 1)
  if (!draft.value.places.some((place) => place.alias === draft.value?.defaultAlias)) {
    draft.value.defaultAlias = ''
  }
}

async function search(): Promise<void> {
  const q = query.value.trim()
  if (!q) return
  searching.value = true
  searchError.value = ''
  results.value = []
  try {
    const response = await geocodePlace(q)
    results.value = response.results ?? []
    if (!results.value.length) searchError.value = response.detail || t('weatherSearchEmpty')
  } catch (err) {
    searchError.value = errorDetail(err)
  } finally {
    searching.value = false
  }
}

function useGeocoded(place: GeocodePlace): void {
  if (!draft.value) return
  const taken = new Set(draft.value.places.map((item) => item.alias))
  let alias = place.name
  let suffix = 2
  while (taken.has(alias)) alias = `${place.name}${suffix++}`
  draft.value.places.push({
    alias,
    name: [place.name, place.admin].filter(Boolean).join(' · '),
    latitude: place.latitude,
    longitude: place.longitude,
    timezone: place.timezone || 'Asia/Shanghai',
    city_code: place.city_code ?? '',
  })
  if (!draft.value.defaultAlias) draft.value.defaultAlias = alias
  results.value = []
  query.value = ''
}

async function save(): Promise<void> {
  if (!draft.value) return
  const current = draft.value
  await run(async () => {
    const status = await putWeather(
      {
        enabled: current.enabled,
        provider: current.provider,
        endpoint: current.endpoint.trim(),
        api_key_env: current.apiKeyEnv.trim(),
        places: current.places.map((place) => ({
          ...place,
          alias: place.alias.trim(),
          name: place.name.trim(),
          timezone: place.timezone.trim(),
          city_code: (place.city_code ?? '').trim(),
        })),
        default_alias: current.defaultAlias,
        max_days: current.maxDays,
        ...(current.apiKey ? { api_key: current.apiKey } : {}),
        revision: current.revision || undefined,
      },
      { timeoutMs: 10000 },
    )
    // The server returns the new revision and the normalised places; adopt
    // them so the next save cannot conflict and the form shows what is stored.
    draft.value = { ...fromStatus(status), apiKey: '' }
    await weather.load()
  })
}

async function runProbe(): Promise<void> {
  probing.value = true
  probe.value = null
  probeError.value = ''
  try {
    probe.value = await testWeather()
  } catch (err) {
    probeError.value = errorDetail(err)
  } finally {
    probing.value = false
  }
}

function reload(): void {
  void weather.load().then(() => {
    if (weather.data.value) draft.value = fromStatus(weather.data.value)
  })
  probe.value = null
  probeError.value = ''
}
</script>

<template>
  <Panel
    icon="weather" apply="immediate" :title="t('weatherTitle')" :hint="t('weatherHint')" :tone="weather.data.value?.state === 'ready' ? 'default' : 'warn'">
    <template #actions>
      <Pill :tone="stateTone" size="sm">{{ stateLabel }}</Pill>
      <Pill v-if="weather.data.value" size="sm" mono>
        {{ weather.data.value.places.length }} {{ t('weatherPlaces') }}
      </Pill>
    </template>

    <ErrorNotice
      v-if="weather.error.value"
      :error="weather.error.value"
      compact
      @retry="weather.load()"
    />

    <template v-if="draft">
      <Toggle v-model="draft.enabled" :label="t('weatherEnabled')" />

      <div class="weather__grid">
        <Select v-model="draft.provider" :label="t('weatherProvider')" :options="providerOptions" />
        <TextInput
          v-model="draft.endpoint"
          :label="t('weatherEndpoint')"
          wide
          :placeholder="endpointPlaceholder"
        />
      </div>

      <div class="weather__search">
        <label class="weather__search-label" for="weather-city">{{ t('weatherSearch') }}</label>
        <div class="weather__search-row">
          <input
            id="weather-city"
            v-model="query"
            class="wf__input weather__search-input"
            :placeholder="t('weatherSearchPlaceholder')"
            @keydown.enter.prevent="search"
          />
          <button type="button" class="btn" :disabled="searching || !query.trim()" @click="search">
            <AppIcon name="weather" :size="13" /> {{ searching ? t('loading') : t('weatherSearch') }}
          </button>
        </div>
        <p v-if="searchError" class="weather__msg weather__msg--error">{{ searchError }}</p>
        <ul v-if="results.length" class="weather__results">
          <li v-for="(item, index) in results" :key="`geo-${index}`" class="weather__result">
            <span>{{ item.label }}</span>
            <span class="mono weather__coords">
              {{ formatNumber(item.latitude, 4) }}, {{ formatNumber(item.longitude, 4) }}
            </span>
            <button type="button" class="btn" @click="useGeocoded(item)">{{ t('weatherPick') }}</button>
          </li>
        </ul>
      </div>

      <div class="weather__places">
        <p class="weather__subtitle">{{ t('weatherPlaces') }}</p>
        <div v-for="(place, index) in draft.places" :key="`place-${index}`" class="weather__place">
          <label class="wf">
            <span class="wf__label">{{ t('weatherAlias') }}</span>
            <input v-model="place.alias" class="wf__input" :placeholder="t('weatherAliasPlaceholder')" />
          </label>
          <label class="wf">
            <span class="wf__label">{{ t('weatherPlaceName') }}</span>
            <input v-model="place.name" class="wf__input" />
          </label>
          <label class="wf">
            <span class="wf__label">{{ t('weatherLat') }}</span>
            <input v-model.number="place.latitude" class="wf__input mono" type="number" step="0.0001" />
          </label>
          <label class="wf">
            <span class="wf__label">{{ t('weatherLon') }}</span>
            <input v-model.number="place.longitude" class="wf__input mono" type="number" step="0.0001" />
          </label>
          <label class="wf">
            <span class="wf__label">{{ t('weatherTimezone') }}</span>
            <input v-model="place.timezone" class="wf__input mono" placeholder="Asia/Shanghai" />
          </label>
          <label class="wf">
            <span class="wf__label">{{ t('weatherCityCode') }}</span>
            <input v-model="place.city_code" class="wf__input mono" placeholder="101070101" inputmode="numeric" />
          </label>
          <button type="button" class="btn btn--danger" @click="removePlace(index)">
            {{ t('weatherRemovePlace') }}
          </button>
        </div>
        <div>
          <button type="button" class="btn" @click="addPlace">{{ t('weatherAddPlace') }}</button>
        </div>
      </div>

      <!-- Audit R2-17: the endpoint, key and limits are infrastructure. The
           thing a person comes here to do is make it know their city, so the
           search comes first and this stays collapsed. -->
      <details class="weather__advanced">
        <summary>{{ t('weatherAdvanced') }}</summary>
        <div class="weather__grid">
          <TextInput v-model="draft.apiKeyEnv" :label="t('apiKeyEnv')" placeholder="WEATHER_API_KEY" />
          <PasswordField
            v-model="draft.apiKey"
            :label="t('weatherApiKey')"
            :is-set="weather.data.value?.api_key_set ?? false"
            :hint="t('weatherApiKeySet')"
          />
          <NumberField v-model="draft.maxDays" :label="t('weatherMaxDays')" :min="1" :max="7" />
          <Select v-model="draft.defaultAlias" :label="t('weatherDefaultAlias')" :options="aliasOptions" />
        </div>
      </details>

      <div class="weather__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
        </button>
        <button type="button" class="btn" :disabled="probing" @click="runProbe">
          <AppIcon name="probes" :size="13" /> {{ probing ? t('testing') : t('weatherTest') }}
        </button>
        <button type="button" class="btn" @click="reload"><AppIcon name="reload" :size="13" /> {{ t('reload') }}</button>
        <span v-if="message" class="weather__msg" :class="`weather__msg--${tone}`">{{ message }}</span>
      </div>

      <div v-if="probe" class="weather__probe">
        <Pill :tone="probe.ok ? 'ok' : 'danger'" size="sm" mono>
          {{
            probe.ok
              ? t('weatherTestOk', {
                  place: probe.place ?? '',
                  temp: probe.temperature_c ?? '',
                  condition: probe.condition ?? '',
                  ms: probe.latency_ms ?? 0,
                })
              : t('weatherTestFail', { detail: probe.detail || probe.error || probe.state })
          }}
        </Pill>
      </div>
      <p v-if="probeError" class="weather__msg weather__msg--error">{{ probeError }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.weather__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.weather__places {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.weather__subtitle {
  font-size: var(--fs-sm);
  font-weight: 650;
  color: var(--ink-2);
}

.weather__place {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  gap: var(--sp-2);
  align-items: end;
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
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
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.24rem var(--sp-2);
}

.weather__search {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px dashed var(--line-strong);
  border-radius: var(--r-sm);
}

.weather__search-label {
  font-size: var(--fs-sm);
  color: var(--ink-2);
}

.weather__search-row {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.weather__search-input {
  width: 16rem;
}

.weather__results {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.weather__result {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
  font-size: var(--fs-sm);
}

.weather__coords {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.weather__actions {
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

.btn--danger {
  border-color: var(--danger-line);
  color: var(--danger);
  align-self: end;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.weather__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.weather__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.muted {
  color: var(--muted);
  font-size: var(--fs-sm);
}
</style>
