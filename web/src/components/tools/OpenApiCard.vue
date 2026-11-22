<script setup lang="ts">
/**
 * OpenAPI query adapter (R2-BM6): pick a document, discover its GET
 * operations, whitelist a few. The whitelist is resolved against the REAL
 * document when the card saves; the model can only call what is stored.
 *
 * v1 is read-only on purpose: an arbitrary write has no read-back contract,
 * so it could not produce the core-owned action fact. Non-GET endpoints are
 * shown as unavailable, not hidden.
 */
import { computed, ref, watch } from 'vue'
import {
  discoverOpenApi,
  getOpenApi,
  putOpenApi,
  testOpenApi,
  type DiscoveredOperation,
  type OpenApiEffectLevel,
  type OpenApiOperation,
  type OpenApiOperationSelection,
  type OpenApiProbe,
  type OpenApiStatus,
  type OpenApiVerificationStrategy,
} from '../../api'
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
import PasswordField from '../PasswordField.vue'
import TextInput from '../TextInput.vue'
import NumberField from '../NumberField.vue'

const openapi = useResource((signal) => getOpenApi({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => openapi.load(), 15000)

interface Draft {
  enabled: boolean
  specUrl: string
  baseUrl: string
  authHeader: string
  token: string
  timeoutMs: number
  retries: number
  revision: string
}

interface OpConfigDraft {
  effectLevel: OpenApiEffectLevel
  verifyType: 'transport_only' | 'readback_op' | 'response_assert'
  opId: string
  jsonPath: string
  expectedRaw: string
}

const draft = ref<Draft | null>(null)
const discovered = ref<DiscoveredOperation[]>([])
const selected = ref<string[]>([])
const opConfigs = ref<Record<string, OpConfigDraft>>({})
const discovering = ref(false)
const discoveryError = ref('')
const discoveryNote = ref('')
const probe = ref<OpenApiProbe | null>(null)
const probing = ref(false)
const probeError = ref('')
const { busy, message, tone, run } = useSave()

function formatExpected(val: unknown): string {
  if (val === undefined) return ''
  if (typeof val === 'string') return val
  try {
    return JSON.stringify(val)
  } catch {
    return String(val)
  }
}

function parseExpected(raw: string): unknown {
  const s = raw.trim()
  if (s === 'true') return true
  if (s === 'false') return false
  if (s === 'null') return null
  if (s !== '' && !Number.isNaN(Number(s))) return Number(s)
  if ((s.startsWith('{') && s.endsWith('}')) || (s.startsWith('[') && s.endsWith(']'))) {
    try {
      return JSON.parse(s)
    } catch {
      return s
    }
  }
  return s
}

function defaultOpConfig(op: Pick<OpenApiOperation, 'method' | 'effect_level' | 'verification'>): OpConfigDraft {
  const isGet = op.method.toLowerCase() === 'get'
  const effectLevel: OpenApiEffectLevel = op.effect_level ?? (isGet ? 'query' : 'mutate')
  const v = op.verification
  if (v?.type === 'readback_op') {
    return {
      effectLevel,
      verifyType: 'readback_op',
      opId: v.op_id,
      jsonPath: v.json_path,
      expectedRaw: formatExpected(v.expected),
    }
  }
  if (v?.type === 'response_assert') {
    return {
      effectLevel,
      verifyType: 'response_assert',
      opId: '',
      jsonPath: v.json_path,
      expectedRaw: formatExpected(v.expected),
    }
  }
  return {
    effectLevel,
    verifyType: 'transport_only',
    opId: '',
    jsonPath: '',
    expectedRaw: '',
  }
}

function syncOpConfigs(ops: Array<Pick<OpenApiOperation, 'id' | 'method' | 'effect_level' | 'verification'>>): void {
  const next = { ...opConfigs.value }
  for (const op of ops) {
    if (!next[op.id] || op.verification !== undefined) {
      next[op.id] = defaultOpConfig(op)
    }
  }
  opConfigs.value = next
}

function getOpConfig(op: DiscoveredOperation): OpConfigDraft {
  if (!opConfigs.value[op.id]) {
    opConfigs.value[op.id] = defaultOpConfig(op)
  }
  return opConfigs.value[op.id]
}

function isWriteOp(op: DiscoveredOperation): boolean {
  return op.method.toLowerCase() !== 'get'
}

const getOperations = computed(() =>
  discovered.value.filter((op) => op.callable && op.method.toLowerCase() === 'get'),
)

function fromStatus(status: OpenApiStatus): Draft {
  return {
    enabled: status.enabled,
    specUrl: status.spec_url ?? '',
    baseUrl: status.base_url ?? '',
    authHeader: status.auth_header ?? '',
    token: '',
    timeoutMs: status.timeout_ms ?? 5000,
    retries: status.retries ?? 1,
    revision: status.revision,
  }
}

watch(
  () => openapi.data.value,
  (status) => {
    if (!status) return
    if (draft.value === null) draft.value = fromStatus(status)
    // The stored catalogue is what the model may call; show it even before a
    // new discovery, so the card never looks empty while it is configured.
    if (discovered.value.length === 0 && status.operations.length) {
      discovered.value = status.operations.map((op) => ({ ...op, callable: true }))
      selected.value = status.operations.map((op) => op.id)
      syncOpConfigs(status.operations)
    }
  },
  { immediate: true },
)

const stateLabel = computed(() => {
  switch (openapi.data.value?.state) {
    case 'ready':
      return t('openapiStateReady')
    case 'disabled':
      return t('openapiStateDisabled')
    default:
      return t('openapiStateUnconfigured')
  }
})

const stateTone = computed(() => {
  switch (openapi.data.value?.state) {
    case 'ready':
      return 'ok' as const
    case 'disabled':
      return 'warn' as const
    default:
      return 'info' as const
  }
})

const callableCount = computed(() => discovered.value.filter((op) => op.callable).length)

function toggle(id: string): void {
  const at = selected.value.indexOf(id)
  if (at >= 0) selected.value.splice(at, 1)
  else selected.value.push(id)
}

function isSelected(id: string): boolean {
  return selected.value.includes(id)
}

function selectAll(): void {
  selected.value = discovered.value.filter((op) => op.callable).map((op) => op.id)
}

function paramsText(op: DiscoveredOperation): string {
  if (!op.params.length) return t('openapiNoParams')
  return op.params
    .map((p) => `${p.name}:${p.kind}${p.required ? '!' : ''}`)
    .join(' · ')
}

async function discover(): Promise<void> {
  if (!draft.value) return
  const url = draft.value.specUrl.trim()
  if (!url) {
    discoveryError.value = t('openapiSpecRequired')
    return
  }
  discovering.value = true
  discoveryError.value = ''
  discoveryNote.value = ''
  try {
    const result = await discoverOpenApi(url, draft.value.token.trim() || undefined)
    if (!result.ok) {
      discoveryError.value = `${result.code ?? 'OPENAPI_SPEC'} · ${result.detail ?? ''}`
      return
    }
    discovered.value = result.operations ?? []
    syncOpConfigs(discovered.value)
    const available = new Set(discovered.value.filter((op) => op.callable).map((op) => op.id))
    const before = selected.value.length
    selected.value = selected.value.filter((id) => available.has(id))
    if (before > 0 && selected.value.length < before) {
      discoveryNote.value = t('openapiSelectionTrimmed')
    }
  } catch (err) {
    discoveryError.value = errorDetail(err)
  } finally {
    discovering.value = false
  }
}

function buildSelection(id: string): OpenApiOperationSelection {
  const op = discovered.value.find((o) => o.id === id)
  const cfg = opConfigs.value[id]
  if (!op || !cfg) return id
  let verification: OpenApiVerificationStrategy | null = null
  if (isWriteOp(op)) {
    if (cfg.verifyType === 'readback_op') {
      verification = {
        type: 'readback_op',
        op_id: cfg.opId.trim(),
        json_path: cfg.jsonPath.trim(),
        expected: parseExpected(cfg.expectedRaw),
      }
    } else if (cfg.verifyType === 'response_assert') {
      verification = {
        type: 'response_assert',
        json_path: cfg.jsonPath.trim(),
        expected: parseExpected(cfg.expectedRaw),
      }
    } else {
      verification = { type: 'transport_only' }
    }
  }
  return {
    id,
    effect_level: cfg.effectLevel,
    ...(verification ? { verification } : {}),
  }
}

async function save(): Promise<void> {
  if (!draft.value) return
  const current = draft.value
  await run(async () => {
    const status = await putOpenApi(
      {
        enabled: current.enabled,
        spec_url: current.specUrl.trim(),
        base_url: current.baseUrl.trim(),
        auth_header: current.authHeader.trim(),
        ...(current.token ? { token: current.token } : {}),
        operations: selected.value.map(buildSelection),
        timeout_ms: current.timeoutMs,
        retries: current.retries,
        revision: current.revision || undefined,
      },
      { timeoutMs: 20000 },
    )
    draft.value = { ...fromStatus(status), token: '' }
    // Keep the full discovered list when there is one: saving must not shrink
    // the picker to the saved subset (adding an operation later would then
    // require re-reading the document for no reason).
    if (discovered.value.length === 0) {
      discovered.value = status.operations.map((op) => ({ ...op, callable: true }))
    }
    syncOpConfigs(status.operations)
    selected.value = status.operations.map((op) => op.id)
    await openapi.load()
  })
}

async function runProbe(): Promise<void> {
  probing.value = true
  probe.value = null
  probeError.value = ''
  try {
    probe.value = await testOpenApi()
  } catch (err) {
    probeError.value = errorDetail(err)
  } finally {
    probing.value = false
  }
}

function reload(): void {
  void openapi.load().then(() => {
    if (openapi.data.value) {
      draft.value = fromStatus(openapi.data.value)
      discovered.value = openapi.data.value.operations.map((op) => ({ ...op, callable: true }))
      selected.value = openapi.data.value.operations.map((op) => op.id)
    }
  })
  probe.value = null
  probeError.value = ''
  discoveryError.value = ''
  discoveryNote.value = ''
}
</script>

<template>
  <Panel
    icon="json"
    apply="next-turn"
    :title="t('openapiTitle')"
    :hint="t('openapiHint')"
    :tone="openapi.data.value?.state === 'ready' ? 'default' : 'warn'"
  >
    <template #actions>
      <Pill :tone="stateTone" size="sm">{{ stateLabel }}</Pill>
      <Pill v-if="openapi.data.value" size="sm" mono>
        {{ openapi.data.value.operations.length }} {{ t('openapiWhitelist') }}
      </Pill>
    </template>

    <ErrorNotice v-if="openapi.error.value" :error="openapi.error.value" compact @retry="openapi.load()" />

    <template v-if="draft">
      <Toggle v-model="draft.enabled" :label="t('openapiEnabled')" />

      <div class="oa__grid">
        <TextInput
          v-model="draft.specUrl"
          :label="t('openapiSpecUrl')"
          wide
          placeholder="https://api.example.com/openapi.json"
        />
        <TextInput v-model="draft.baseUrl" :label="t('openapiBaseUrl')" wide :placeholder="t('openapiBasePlaceholder')" />
        <TextInput
          v-model="draft.authHeader"
          :label="t('openapiAuthHeader')"
          :placeholder="t('openapiAuthHeaderPlaceholder')"
          :hint="t('openapiAuthHeaderHint')"
        />
        <PasswordField
          v-model="draft.token"
          :label="t('openapiToken')"
          :is-set="openapi.data.value?.token_set ?? false"
          :hint="t('openapiTokenHint')"
        />
        <NumberField v-model="draft.timeoutMs" :label="t('openapiTimeout')" :min="500" :max="15000" />
        <NumberField v-model="draft.retries" :label="t('openapiRetries')" :min="0" :max="3" />
      </div>

      <div class="oa__discover">
        <button type="button" class="btn" :disabled="discovering || !draft.specUrl.trim()" @click="discover">
          <AppIcon name="json" :size="13" />
          {{ discovering ? t('openapiDiscovering') : t('openapiDiscover') }}
        </button>
        <span class="oa__msg">{{ t('openapiDiscoverHint') }}</span>
      </div>
      <p v-if="discoveryError" class="oa__msg oa__msg--error">{{ discoveryError }}</p>
      <p v-if="discoveryNote" class="oa__msg">{{ discoveryNote }}</p>

      <div v-if="discovered.length" class="oa__ops">
        <div class="oa__ops-head">
          <p class="oa__subtitle">{{ t('openapiOperations') }}</p>
          <button type="button" class="btn" @click="selectAll">
            {{ t('openapiSelectAll', { n: callableCount }) }}
          </button>
        </div>
        <ul class="oa__op-list">
          <li v-for="op in discovered" :key="op.id" class="oa__op" :class="{ 'oa__op--off': !op.callable }">
            <div class="oa__op-main">
              <label class="oa__check">
                <input
                  type="checkbox"
                  :checked="isSelected(op.id)"
                  :disabled="!op.callable"
                  @change="toggle(op.id)"
                />
                <span class="mono">{{ op.id }}</span>
              </label>
              <Pill size="sm" mono :tone="isWriteOp(op) ? 'warn' : 'info'">{{ op.method.toUpperCase() }}</Pill>
              <span class="mono oa__path">{{ op.path }}</span>
              <span class="oa__summary">{{ op.summary }}</span>
              <span class="mono oa__params">{{ paramsText(op) }}</span>
              <Pill v-if="!op.callable" tone="warn" size="sm">{{ t('openapiNotCallable') }}</Pill>
              <span v-if="!op.callable && op.reason" class="oa__msg oa__msg--error">{{ op.reason }}</span>
            </div>
            <div v-if="op.callable && isSelected(op.id)" class="oa__op-cfg">
              <label class="oa__field">
                <span>{{ t('openapiEffectLevel') }}</span>
                <select v-model="getOpConfig(op).effectLevel" class="oa__select">
                  <option value="query">{{ t('openapiEffectQuery') }}</option>
                  <option value="mutate">{{ t('openapiEffectMutate') }}</option>
                  <option value="destructive">{{ t('openapiEffectDestructive') }}</option>
                </select>
              </label>
              <template v-if="isWriteOp(op)">
                <label class="oa__field">
                  <span>{{ t('openapiVerifyStrategy') }}</span>
                  <select v-model="getOpConfig(op).verifyType" class="oa__select">
                    <option value="transport_only">{{ t('openapiVerifyTransportOnly') }}</option>
                    <option value="readback_op">{{ t('openapiVerifyReadbackOp') }}</option>
                    <option value="response_assert">{{ t('openapiVerifyResponseAssert') }}</option>
                  </select>
                </label>
                <label v-if="getOpConfig(op).verifyType === 'readback_op'" class="oa__field">
                  <span>{{ t('openapiVerifyReadbackTarget') }}</span>
                  <select v-model="getOpConfig(op).opId" class="oa__select">
                    <option value="">{{ t('openapiSelectGetOp') }}</option>
                    <option v-for="g in getOperations" :key="g.id" :value="g.id">{{ g.id }}</option>
                  </select>
                </label>
                <template v-if="getOpConfig(op).verifyType !== 'transport_only'">
                  <label class="oa__field">
                    <span>{{ t('openapiVerifyJsonPath') }}</span>
                    <input v-model="getOpConfig(op).jsonPath" type="text" class="oa__input mono" placeholder="status" />
                  </label>
                  <label class="oa__field">
                    <span>{{ t('openapiVerifyExpected') }}</span>
                    <input v-model="getOpConfig(op).expectedRaw" type="text" class="oa__input mono" placeholder="ok / true" />
                  </label>
                </template>
                <span v-else class="oa__msg">{{ t('openapiVerifyTransportNote') }}</span>
              </template>
            </div>
          </li>
        </ul>
        <p v-if="!callableCount" class="oa__msg">{{ t('openapiNoCallable') }}</p>
      </div>
      <p v-else class="oa__msg">{{ t('openapiDiscoverFirst') }}</p>

      <div class="oa__actions">
        <button
          type="button"
          class="btn btn--primary"
          :disabled="busy"
          :title="!selected.length ? t('openapiSaveEmptyHint') : ''"
          @click="save"
        >
          <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
        </button>
        <button type="button" class="btn" :disabled="probing" @click="runProbe">
          <AppIcon name="probes" :size="13" /> {{ probing ? t('testing') : t('openapiTest') }}
        </button>
        <button type="button" class="btn" @click="reload"><AppIcon name="reload" :size="13" /> {{ t('reload') }}</button>
        <span v-if="message" class="oa__msg" :class="`oa__msg--${tone}`">{{ message }}</span>
      </div>
      <p v-if="!selected.length" class="oa__msg">{{ t('openapiPickRequired') }}</p>

      <div v-if="probe" class="oa__probe">
        <Pill :tone="probe.ok ? 'ok' : 'danger'" size="sm" mono>
          {{
            probe.ok
              ? t('openapiProbeSummary', {
                  n: probe.operations,
                  requests: probe.http_requests,
                  ms: probe.latency_ms,
                })
              : t('openapiProbeFail', { detail: probe.detail || probe.code || probe.state })
          }}
        </Pill>
        <span v-if="probe.sample?.operation" class="oa__msg mono">
          {{ t('openapiProbeSample', { op: probe.sample.operation, status: probe.sample.status ?? '' }) }}
        </span>
      </div>
      <p v-if="probeError" class="oa__msg oa__msg--error">{{ probeError }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.oa__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.oa__discover,
.oa__actions,
.oa__probe {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
}

.oa__ops {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.oa__ops-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-2);
}

.oa__subtitle {
  font-size: var(--fs-sm);
  font-weight: 650;
  color: var(--ink-2);
}

.oa__op-list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  margin: 0;
  padding: 0;
  list-style: none;
}

.oa__op {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.oa__op-main,
.oa__op-cfg {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
}

.oa__op-cfg {
  padding-top: var(--sp-1);
  border-top: 1px dashed var(--line);
}

.oa__field {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  font-size: var(--fs-xs);
  color: var(--ink-2);
}

.oa__select,
.oa__input {
  padding: 0.2rem 0.45rem;
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface);
  color: var(--ink);
  font-size: var(--fs-xs);
}

.oa__op--off {
  opacity: 0.65;
}

.oa__check {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-2);
  min-width: 12rem;
}

.oa__path {
  color: var(--ink-2);
}

.oa__params {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.oa__summary {
  color: var(--muted);
  font-size: var(--fs-sm);
}

.oa__msg {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.oa__msg--error {
  color: var(--danger);
}

.oa__msg--ok {
  color: var(--ok);
}
</style>
