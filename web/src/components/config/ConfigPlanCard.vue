<script setup lang="ts">
import { computed, ref, shallowRef, watch } from 'vue'
import {
  generateDot2dPersonaProfile, type Dot2dPersonaProfile,
  generateDot2dFamilyProfile, type Dot2dFamilyProfile,
  commitConfig, enrollConfigNode, exportConfig, type ConfigExportResponse, getConfigApplication, getConfigState, preflightConfig, getConfigReadback, previewConfigRollback,
  type ConfigApplication, type ConfigPublication, type ConfigReadback, type ConfigCommit, type ConfigField, type ConfigOperation,
  type ConfigPackage, type ConfigPlan, type ConfigPurpose, type ConfigValue, newConfigIdentity,
} from '../../api/configV2'
import { ApiError } from '../../api/client'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import ErrorNotice from '../ErrorNotice.vue'
import LoadingBlock from '../LoadingBlock.vue'

const state = useResource(signal => getConfigState({ signal }), { retries: 0 })
const purpose = ref<ConfigPurpose>('node_config')
const reason = ref('')
type EditAction = 'set' | 'replace' | 'disable' | 'null' | 'clear'
const edits = shallowRef<Record<string, { action: EditAction; value: ConfigValue }>>({})
const requiredBindings = ref<string[]>([])
const plan = shallowRef<ConfigPlan | null>(null)
const submission = ref<ConfigCommit | null>(null)
const result = shallowRef<ConfigApplication | null>(null)
const profile = shallowRef<Dot2dPersonaProfile | null>(null)
const familyProfile = shallowRef<Dot2dFamilyProfile | null>(null)
const resultError = ref<unknown>(null)
const readback = shallowRef<ConfigReadback | null>(null)
const readbackError = ref<unknown>(null)
const publications = computed(() => readback.value?.publications ?? (readback.value?.publication ? [readback.value.publication] : []))
let readbackRequest = 0
const resultReadAt = ref<number | null>(null)
const error = ref<unknown>(null)
const busy = ref(false)
const unknownOutcome = ref(false)
const now = ref(Date.now())
const dirty = computed(() => Object.keys(edits.value).length > 0 || !!reason.value || requiredBindings.value.length > 0)
const ready = computed(() => !!state.data.value && !state.error.value && !state.loading.value && state.data.value.target.enrolled)
const applicable = computed(() => (state.data.value?.fields ?? []).filter(field => {
  const block = field.path.split('.')[0]
  const caps = state.data.value?.capabilities
  return field.supported && caps?.supported_blocks.includes(block) && ['family_or_node', 'node'].includes(field.scope) && field.purposes.includes(purpose.value) && !caps.unavailable_blocks?.includes(block) && (block !== 'weather' || caps.weather_reviewable !== false)
}))
const editable = computed(() => applicable.value.filter(field => isMusicConnection(field.path) || catalogueLabels(field.path) || ['string', 'boolean', 'number', 'integer'].includes(field.kind)))
const protectedFields = computed(() => (state.data.value?.fields ?? []).filter(field => !applicable.value.includes(field)))
const locked = computed(() => busy.value || unknownOutcome.value)
const canCommit = computed(() => ready.value && !!plan.value && !plan.value.blocked.length && plan.value.expires_ms > now.value && !locked.value)
const stateFingerprint = computed(() => state.data.value ? JSON.stringify([state.data.value.target.household_id, state.data.value.target.node_id, state.data.value.target.enrolled, state.data.value.data_epoch, state.data.value.config_revision]) : null)
watch(stateFingerprint, (next, previous) => {
  if (previous === null || next === previous) return
  plan.value = null
  profile.value = null
  familyProfile.value = null
  if (!unknownOutcome.value) submission.value = null
}, { flush: 'sync' })
const applications = computed(() => state.data.value?.applications ?? [])
const protectedFamilies = computed(() => [t('cfgProtectedAudio'), t('cfgProtectedHardware'), t('cfgProtectedTools'), t('cfgProtectedData')])
const exported = shallowRef<ConfigExportResponse | null>(null)
const exportBusy = ref(false)
const registryQuery = ref('')
const registryScope = ref('all')
const allFields = computed(() => state.data.value?.fields ?? [])
const filteredRegistry = computed(() => {
  const q = registryQuery.value.trim().toLowerCase()
  return allFields.value.filter(field => {
    if (registryScope.value !== 'all' && field.scope !== registryScope.value) return false
    if (!q) return true
    return field.path.toLowerCase().includes(q) || field.validator.toLowerCase().includes(q) || field.kind.toLowerCase().includes(q)
  })
})

watch(purpose, () => {
  const allowed = new Set(editable.value.map(field => field.path))
  edits.value = Object.fromEntries(Object.entries(edits.value).filter(([path]) => allowed.has(path)))
}, { flush: 'sync' })

watch([purpose, reason, edits, requiredBindings], () => {
  // An uncertain commit retains its original identity until queried/retried.
  if (unknownOutcome.value) return
  profile.value = null
  familyProfile.value = null
  plan.value = null
  submission.value = null
}, { deep: true, flush: 'sync' })

usePoll(async () => {
  now.value = Date.now()
  if (unknownOutcome.value && submission.value) await readSubmission()
  else if (publications.value.some(value => value.state === 'saved_pending') && !readbackError.value) await refreshReadback()
}, 1000)
usePoll(() => state.load(), 30000)

function display(value: ConfigValue | undefined): string {
  return value === undefined ? '—' : typeof value === 'string' ? value || '""' : JSON.stringify(value)
}
function readerState(state: string): string {
  const labels = { saved_pending: t('cfgPublicationPending'), applied: t('cfgPublicationApplied'), failed: t('cfgPublicationFailed'), core_readback_verified: t('cfgReaderVerified'), binding_name_readback: t('cfgReaderBinding'), shadowed: t('cfgReaderShadowed'), inactive: t('cfgReaderInactive'), not_consumed: t('cfgReaderUnused'), pending_dependency: t('cfgReaderDependency'), reader_error: t('cfgReaderError'), reader_mismatch: t('cfgReaderMismatch'), superseded: t('cfgReaderSuperseded'), partial_readback: t('cfgReadback') }
  return labels[state as keyof typeof labels] ?? t('cfgReaderUnknown')
}
function publicationLabel(block: ConfigPublication['block']): string {
  const labels = { persona: 'cfgPublication', services: 'cfgServicesPublication', weather: 'cfgWeatherPublication', home: 'cfgHomePublication', music: 'cfgMusicPublication', openapi: 'cfgOpenApiPublication', open_sockets: 'cfgOpenSocketsPublication', mode: 'cfgVoicePublication', news: 'cfgNewsPublication', search: 'cfgSearchPublication', node: 'cfgNodePublication', device_led_configs: 'cfgDeviceLedPublication' } as const
  return labels[block as keyof typeof labels] ? t(labels[block as keyof typeof labels]) : block
}
function isNumeric(field: ConfigField): boolean {
  return field.kind === 'number' || field.kind === 'integer'
}
function voiceNullLabel(path: string): string | null {
  if (path === 'mode.followup_credits') return t('cfgFollowupUnlimited')
  if (path === 'mode.system_prompt_override') return t('cfgSystemPromptDefault')
  return null
}
function isMusicConnection(path: string): boolean {
  return path === 'music.library' || path === 'music.online'
}
function musicPart(value: ConfigValue, key: 'endpoint' | 'user'): string {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return ''
  const part = value[key]
  return typeof part === 'string' ? part : ''
}
function setMusicPart(path: string, key: 'endpoint' | 'user', next: string): void {
  const value = edits.value[path].value
  setValue(path, { endpoint: musicPart(value, 'endpoint'), user: musicPart(value, 'user'), [key]: next })
}
function catalogueLabels(path: string) {
  if (path === 'openapi.operations') return { label: 'cfgOpenApiOperations', action: 'cfgOpenApiReplaceCatalog', invalid: 'cfgOpenApiCatalogInvalid' } as const
  if (path === 'open_sockets') return { label: 'cfgOpenSocketsContracts', action: 'cfgOpenSocketsReplace', invalid: 'cfgOpenSocketsInvalid' } as const
  if (['mode.retreat_words', 'mode.stop_intents.stop_words', 'mode.stop_intents.continue_words'].includes(path)) {
    return { label: 'cfgVoiceWordList', action: 'cfgVoiceReplaceWords', invalid: 'cfgVoiceListInvalid' } as const
  }
}
function layerDisplay(value: ConfigValue, explicitNull = false): string {
  if (explicitNull) return display(null)
  return value === null ? t('cfgInherit') : display(value)
}
function choose(field: ConfigField, selected: boolean): void {
  const path = field.path
  if (selected) {
    const current = state.data.value?.resolved[path] ?? null
    let edit: { action: EditAction; value: ConfigValue }
    if (isMusicConnection(path)) {
      edit = { action: 'replace', value: { endpoint: musicPart(current, 'endpoint'), user: musicPart(current, 'user') } }
    } else if (catalogueLabels(path)) {
      edit = { action: 'replace', value: JSON.stringify(current ?? [], null, 2) }
    } else {
      edit = { action: field.nullable && current === null && voiceNullLabel(path) ? 'null' : 'set', value: current ?? (field.kind === 'boolean' ? false : isNumeric(field) ? 0 : '') }
    }
    edits.value = { ...edits.value, [path]: edit }
  } else {
    const next = { ...edits.value }
    delete next[path]
    edits.value = next
  }
}
function setAction(path: string, action: string): void {
  if (action !== 'set' && action !== 'replace' && action !== 'disable' && action !== 'null' && action !== 'clear') return
  edits.value = { ...edits.value, [path]: { ...edits.value[path], action } }
}
function setValue(path: string, value: ConfigValue): void {
  edits.value = { ...edits.value, [path]: { ...edits.value[path], value } }
}
function binding(id: string, required: boolean): void {
  requiredBindings.value = required ? [...requiredBindings.value, id] : requiredBindings.value.filter(value => value !== id)
}
async function reload(): Promise<void> {
  if (locked.value) return
  await state.load()
  if (state.error.value) return
  edits.value = {}; requiredBindings.value = []; reason.value = ''; plan.value = null; submission.value = null
}
async function enroll(): Promise<void> {
  if (busy.value || !state.data.value?.capabilities.can_enroll) return
  busy.value = true; error.value = null
  try { await enrollConfigNode(); await state.load() } catch (err) { error.value = err } finally { busy.value = false }
}
function makePackage(): ConfigPackage {
  const current = state.data.value!
  const groups = new Map<string, ConfigOperation[]>()
  for (const [path, edit] of Object.entries(edits.value)) {
    const name = path.split('.')[0]
    const ops = groups.get(name) ?? []
    if (edit.action === 'clear') ops.push({ action: 'clear', path })
    else if (edit.action === 'disable' || edit.action === 'null') ops.push({ action: 'set', path, value: null })
    else {
      const labels = catalogueLabels(path)
      if (labels) {
        let items: unknown
        try { items = JSON.parse(String(edit.value)) } catch { throw new Error(t(labels.invalid)) }
        if (!Array.isArray(items)) throw new Error(t(labels.invalid))
        ops.push({ action: 'replace', path, value: items })
      } else ops.push({ action: edit.action, path, value: edit.value })
    }
    groups.set(name, ops)
  }
  return {
    format: 'home-mate-config', version: 2, package_id: newConfigIdentity(), package_revision: 1, created_ms: Date.now(), purpose: purpose.value,
    source: { node_id: current.target.node_id, config_revision: current.config_revision, core_version: current.core_version, schema_version: current.schema_version },
    target: { household_id: current.target.household_id, ...(purpose.value === 'family_policy' ? {} : { node_id: current.target.node_id }) },
    compatibility: { core_api_schema: 2, gateway_protocol_min: 0, gateway_protocol_max: 0, required_capabilities: [] },
    change_reason: reason.value.trim(), blocks: [...groups].map(([name, operations]) => ({ name, schema_version: 1, operations })),
    bindings: current.bindings.map(item => ({ binding_id: item.binding_id, action: requiredBindings.value.includes(item.binding_id) ? 'require_binding' : 'preserve' })),
  }
}
async function reviewDot2d(): Promise<void> {
  if (!ready.value || locked.value || dirty.value || state.data.value?.target.node_id !== 'dot2-d') return
  busy.value = true; error.value = null; plan.value = null; submission.value = null; profile.value = null; familyProfile.value = null
  const baseline = stateFingerprint.value
  try {
    const generated = await generateDot2dPersonaProfile()
    if (stateFingerprint.value !== baseline) { error.value = new Error(t('cfgConflict')); return }
    profile.value = generated; plan.value = generated.preflight; now.value = Date.now()
  } catch (err) {
    error.value = err instanceof ApiError && err.detail === 'PROFILE_BASELINE_PENDING' ? new Error(t('cfgDot2dPending'))
      : err instanceof ApiError && err.detail === 'PROFILE_TARGET_MISMATCH' ? new Error(t('cfgDot2dIdentity')) : err
  } finally { busy.value = false }
}
function downloadDot2d(): void {
  if (!profile.value) return
  const url = URL.createObjectURL(new Blob([JSON.stringify(profile.value.package, null, 2)], { type: 'application/json' }))
  try {
    const anchor = document.createElement('a')
    anchor.href = url; anchor.download = profile.value.filename; anchor.click()
  } finally { URL.revokeObjectURL(url) }
}
async function reviewDot2dFamily(): Promise<void> {
  if (!ready.value || locked.value || dirty.value || state.data.value?.target.node_id !== 'dot2-d') return
  busy.value = true; error.value = null; plan.value = null; submission.value = null; profile.value = null; familyProfile.value = null
  const baseline = stateFingerprint.value
  try {
    const generated = await generateDot2dFamilyProfile()
    if (stateFingerprint.value !== baseline) { error.value = new Error(t('cfgConflict')); return }
    familyProfile.value = generated; plan.value = generated.preflight; now.value = Date.now()
  } catch (err) {
    error.value = err instanceof ApiError && err.detail === 'PROFILE_BASELINE_PENDING' ? new Error(t('cfgDot2dPending'))
      : err instanceof ApiError && err.detail === 'PROFILE_TARGET_MISMATCH' ? new Error(t('cfgDot2dIdentity')) : err
  } finally { busy.value = false }
}
function downloadDot2dFamily(): void {
  if (!familyProfile.value) return
  const url = URL.createObjectURL(new Blob([JSON.stringify(familyProfile.value.package, null, 2)], { type: 'application/json' }))
  try {
    const anchor = document.createElement('a')
    anchor.href = url; anchor.download = familyProfile.value.filename; anchor.click()
  } finally { URL.revokeObjectURL(url) }
}
async function preview(): Promise<void> {
  if (!ready.value || locked.value) return
  if (!Object.keys(edits.value).length || !reason.value.trim()) { error.value = new Error(t('cfgNoChanges')); return }
  busy.value = true; error.value = null; plan.value = null; submission.value = null
  const baseline = stateFingerprint.value
  try {
    const previewed = await preflightConfig(makePackage())
    if (stateFingerprint.value !== baseline) { error.value = new Error(t('cfgConflict')); return }
    plan.value = previewed; now.value = Date.now()
  } catch (err) { error.value = err } finally { busy.value = false }
}
async function readSubmission(): Promise<void> {
  if (!submission.value) return
  try {
    result.value = await getConfigApplication(submission.value.application_id)
    resultReadAt.value = Date.now(); resultError.value = null; unknownOutcome.value = false; plan.value = null
  } catch (err) { resultError.value = err }
}
async function send(): Promise<void> {
  if (busy.value) return
  if (!submission.value) {
    if (!canCommit.value || !plan.value) return
    submission.value = { plan_id: plan.value.plan_id, plan_hash: plan.value.plan_hash, expected_revision: plan.value.expected_revision, application_id: newConfigIdentity() }
  }
  busy.value = true; error.value = null
  try {
    result.value = await commitConfig(submission.value)
    readback.value = null; readbackError.value = null; ++readbackRequest
    resultReadAt.value = Date.now(); resultError.value = null; unknownOutcome.value = false
    // Consumption makes this preview unavailable for another application.
    plan.value = null
    await state.load()
  } catch (err) {
    error.value = err
    if (err instanceof ApiError && ['network', 'timeout', 'bad_response', 'server_error', 'aborted'].includes(err.code)) {
      unknownOutcome.value = true
    } else {
      unknownOutcome.value = false; plan.value = null; submission.value = null
      if (err instanceof ApiError && err.code === 'conflict') error.value = new Error(t('cfgConflict'))
    }
  } finally { busy.value = false }
}
async function refreshReadback(): Promise<void> {
  if (!result.value || busy.value) return
  const id = result.value.application_id
  const request = ++readbackRequest
  readbackError.value = null
  try {
    const current = await getConfigReadback(id)
    if (request === readbackRequest && result.value?.application_id === id) readback.value = current
  } catch (err) { if (request === readbackRequest) readbackError.value = err }
}
async function rollbackPreview(): Promise<void> {
  if (!result.value || !ready.value || locked.value || dirty.value || !readback.value || readbackError.value) return
  busy.value = true; error.value = null; plan.value = null; submission.value = null
  const baseline = stateFingerprint.value
  try {
    const previewed = await previewConfigRollback(result.value.application_id, state.data.value!.config_revision)
    if (stateFingerprint.value !== baseline) { error.value = new Error(t('cfgConflict')); return }
    plan.value = previewed; now.value = Date.now()
  } catch (err) { error.value = err } finally { busy.value = false }
}
async function showApplication(id: string): Promise<void> {
  if (unknownOutcome.value) return
  resultError.value = null
  try { result.value = await getConfigApplication(id); resultReadAt.value = Date.now(); readback.value = null; readbackError.value = null; ++readbackRequest; await refreshReadback() } catch (err) { resultError.value = err }
}
function formatType(field: ConfigField): string {
  const base = field.array_item_kind ? `${field.kind}<${field.array_item_kind}>` : field.kind
  return field.nullable ? `${base} | null` : base
}
function formatBounds(field: ConfigField): string {
  const b = field.bounds
  if (!b) return '—'
  const parts: string[] = []
  if (b.min !== undefined || b.max !== undefined) parts.push(`${b.min ?? '-∞'}..${b.max ?? '∞'}`)
  if (b.max_len !== undefined) parts.push(`len≤${b.max_len}`)
  if (b.max_items !== undefined) parts.push(`items≤${b.max_items}`)
  if (b.allowed?.length) parts.push(b.allowed.join('|'))
  return parts.length ? parts.join(', ') : '—'
}
async function runExport(format: 'v2' | 'v1'): Promise<void> {
  if (exportBusy.value || locked.value) return
  exportBusy.value = true; error.value = null
  try {
    exported.value = await exportConfig(format === 'v2' ? { format, purpose: purpose.value } : { format })
  } catch (err) { error.value = err } finally { exportBusy.value = false }
}
function downloadExport(): void {
  if (!exported.value) return
  const payload = exported.value.format_version === 2 ? exported.value.package : exported.value.document
  const url = URL.createObjectURL(new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' }))
  try {
    const anchor = document.createElement('a')
    anchor.href = url; anchor.download = exported.value.filename; anchor.click()
  } finally { URL.revokeObjectURL(url) }
}
</script>

<template>
  <Panel icon="settings" :title="t('cfgTitle')" :hint="t('cfgHint')">
    <template #actions><button class="btn" type="button" :disabled="locked || state.loading.value" @click="dirty ? reload() : state.load()">{{ dirty ? t('cfgReload') : t('refresh') }}</button></template>
    <ErrorNotice v-if="state.error.value" :error="state.error.value" :stale="!!state.data.value" :at="state.updatedAt.value" @retry="state.load" />
    <LoadingBlock v-if="!state.data.value && state.loading.value" :label="t('loading')" />
    <ErrorNotice v-if="error" :error="error" @retry="unknownOutcome ? readSubmission() : state.load()" />
    <template v-if="state.data.value">
      <p class="identity"><strong>{{ t('cfgTarget') }}:</strong> <code>{{ state.data.value.target.node_id || '—' }}</code> · {{ t('cfgRevision') }} {{ state.data.value.config_revision }} · {{ t('cfgEpoch') }} {{ state.data.value.data_epoch }}</p>
      <section v-if="!state.data.value.target.enrolled" class="cfg-section">
        <p>{{ t('cfgEnrollHint') }}</p>
        <p v-if="!state.data.value.capabilities.can_enroll">{{ t('cfgEnrollMissing') }} <a href="#fleet-identity">{{ t('cfgIdentitySettings') }}</a></p>
        <button class="btn" type="button" :disabled="busy || !state.data.value.capabilities.enrollment || !state.data.value.capabilities.can_enroll" @click="enroll">{{ t('cfgEnroll') }}</button>
      </section>
      <section v-if="state.data.value.target.enrolled && state.data.value.target.node_id === 'dot2-d'" class="cfg-section">
        <h3>{{ t('cfgDot2dTitle') }}</h3><p>{{ t('cfgDot2dScope') }}</p><p>{{ t('cfgDot2dWake') }}</p>
        <div class="actions">
          <button class="btn" type="button" :disabled="!ready || locked || dirty" @click="reviewDot2d">{{ t('cfgDot2dReview') }}</button>
          <button class="btn btn--primary" type="button" :disabled="!ready || locked || dirty" @click="reviewDot2dFamily">{{ t('cfgDot2dFamilyReview') }}</button>
        </div>
        <template v-if="profile">
          <p>{{ t('cfgDot2dPreserved') }}</p><p>{{ t('cfgDot2dUnavailable') }}</p>
          <ul><li v-for="item in profile.unavailable" :key="item.issue"><code>{{ item.paths.join(', ') }}</code> · <a :href="`https://github.com/yes999zc/home-mate/issues/${item.issue}`" target="_blank" rel="noopener noreferrer">#{{ item.issue }}</a></li></ul>
          <button class="btn" type="button" :disabled="locked" @click="downloadDot2d">{{ t('cfgDot2dDownload') }}</button>
        </template>
        <template v-if="familyProfile">
          <h4>{{ t('cfgDot2dFamilyTitle') }}</h4>
          <p>{{ t('cfgDot2dFamilyScope') }}</p>
          <p class="muted">{{ t('cfgDot2dFamilyAssurance', { node: familyProfile.target_assurance.enrolled_core_node_id, lock: familyProfile.target_assurance.immutable_target_lock, hw: familyProfile.target_assurance.physical_device_identity }) }}</p>
          <div>
            <strong>{{ t('cfgDot2dFamilyRetained') }}</strong>
            <ul>
              <li><code>unit_acoustic_profiles.dot2-d</code>: <code>{{ JSON.stringify(familyProfile.retained_unit_refs['unit_acoustic_profiles.dot2-d']) }}</code></li>
              <li><code>audio_eq</code>: <code>{{ JSON.stringify(familyProfile.retained_unit_refs.audio_eq) }}</code></li>
              <li><code>services</code>: <code>{{ JSON.stringify(familyProfile.retained_unit_refs.services) }}</code></li>
              <li><code>bindings</code>: <code>{{ JSON.stringify(familyProfile.retained_unit_refs.bindings) }}</code></li>
            </ul>
          </div>
          <div>
            <strong>{{ t('cfgDot2dFamilyReadbackReq') }}</strong>
            <p class="muted"><code>{{ familyProfile.compatibility.readback_requirements.required_blocks.join(', ') }}</code> · <code>{{ familyProfile.compatibility.readback_requirements.publication_state }}</code></p>
          </div>
          <div>
            <strong>{{ t('cfgDot2dFamilyRollbackInstr') }}</strong>
            <p class="muted"><code>{{ familyProfile.rollback_instructions.preflight_endpoint }}</code> → <code>{{ familyProfile.rollback_instructions.commit_endpoint }}</code> · {{ t('cfgRevision') }} {{ familyProfile.rollback_instructions.expected_revision }}</p>
          </div>
          <div v-if="familyProfile.blockers.length">
            <strong>{{ t('cfgDot2dFamilyBlockers') }}</strong>
            <ul>
              <li v-for="blocker in familyProfile.blockers" :key="blocker.path"><code>{{ blocker.path }}</code> · <strong>{{ blocker.reason }}</strong> — {{ blocker.detail }}</li>
            </ul>
          </div>
          <button class="btn" type="button" :disabled="locked" @click="downloadDot2dFamily">{{ t('cfgDot2dFamilyDownload') }}</button>
        </template>
      </section>
      <fieldset class="cfg-form" :disabled="!ready || locked">
        <label>{{ t('cfgPurpose') }}<select v-model="purpose"><option value="node_config">{{ t('cfgNode') }}</option><option value="family_policy">{{ t('cfgFamily') }}</option></select></label>
        <label>{{ t('cfgReason') }}<input v-model="reason" type="text" maxlength="1000" :placeholder="t('cfgReasonHint')" /></label>
        <p v-if="dirty" role="status">{{ t('cfgDraft') }}</p>
        <h3>{{ t('cfgSupported') }}</h3>
        <div v-for="field in editable" :key="field.path" class="field-row">
          <label class="choose"><input type="checkbox" :checked="!!edits[field.path]" @change="choose(field, ($event.target as HTMLInputElement).checked)" />{{ t('cfgSelect') }} <code>{{ field.path }}</code></label>
          <p class="muted">{{ t('cfgCurrent') }}: {{ display(state.data.value.stored[field.path]) }} · {{ t('cfgOrigin') }}: {{ state.data.value.origins[field.path] || '—' }}</p>
          <template v-if="edits[field.path]">
            <label>{{ t('cfgAction') }}
              <select :aria-label="t('cfgAction')" :value="edits[field.path].action" @change="setAction(field.path, ($event.target as HTMLSelectElement).value)">
                <template v-if="isMusicConnection(field.path)">
                  <option value="replace">{{ t('cfgReplaceMusicConnection') }}</option>
                  <option value="disable">{{ t('cfgDisableMusicConnection') }}</option>
                </template>
                <option v-else-if="catalogueLabels(field.path)" value="replace">{{ t(catalogueLabels(field.path)!.action) }}</option>
                <option v-else value="set">{{ t('cfgSet') }}</option>
                <option v-if="field.nullable && voiceNullLabel(field.path)" value="null">{{ voiceNullLabel(field.path) }}</option>
                <option v-if="field.clear_behavior === 'remove_layer_override'" value="clear">{{ t('cfgClear') }}</option>
              </select>
            </label>
            <template v-if="edits[field.path].action === 'replace' && isMusicConnection(field.path)">
              <label>{{ t('musicEndpoint') }}
                <input type="text" maxlength="4096" :value="musicPart(edits[field.path].value, 'endpoint')" @input="setMusicPart(field.path, 'endpoint', ($event.target as HTMLInputElement).value)" />
              </label>
              <label>{{ t(field.path === 'music.library' ? 'musicUser' : 'musicOnlineUser') }}
                <input type="text" maxlength="4096" :value="musicPart(edits[field.path].value, 'user')" @input="setMusicPart(field.path, 'user', ($event.target as HTMLInputElement).value)" />
              </label>
            </template>
            <label v-if="catalogueLabels(field.path) && edits[field.path].action === 'replace'">{{ t(catalogueLabels(field.path)!.label) }}
              <textarea rows="8" spellcheck="false" :value="String(edits[field.path].value)" @input="setValue(field.path, ($event.target as HTMLTextAreaElement).value)" />
            </label>
            <label v-if="edits[field.path].action === 'set'">{{ t('cfgDesired') }}
              <input v-if="field.kind === 'boolean'" type="checkbox" :checked="edits[field.path].value === true" @change="setValue(field.path, ($event.target as HTMLInputElement).checked)" />
              <textarea v-else-if="field.path === 'persona.agent_md.content' || field.path === 'mode.persona_prompt' || field.path === 'mode.system_prompt_override'" rows="5" :maxlength="field.path === 'persona.agent_md.content' ? 16000 : 4000" :value="String(edits[field.path].value)" @input="setValue(field.path, ($event.target as HTMLTextAreaElement).value)" />
              <input v-else :type="isNumeric(field) ? 'number' : 'text'" :value="String(edits[field.path].value ?? '')" @input="setValue(field.path, isNumeric(field) ? Number(($event.target as HTMLInputElement).value) : ($event.target as HTMLInputElement).value)" />
            </label>
          </template>
        </div>
        <h3>{{ t('cfgBindings') }}</h3>
        <div v-for="item in state.data.value.bindings" :key="item.binding_id" class="binding">
          <code>{{ item.binding_id }}</code> — {{ item.bound ? t('cfgBound') : t('cfgUnbound') }} · {{ t('cfgEnvironmentNotChecked') }}
          <label class="choose"><input type="checkbox" :checked="requiredBindings.includes(item.binding_id)" @change="binding(item.binding_id, ($event.target as HTMLInputElement).checked)" />{{ t('cfgRequireBinding') }}</label>
        </div>
        <p class="muted">{{ t('cfgPreserved') }}</p>
        <button class="btn btn--primary" type="button" :disabled="!ready || locked" @click="preview">{{ t('cfgPreview') }}</button>
      </fieldset>
      <section v-if="plan" class="cfg-section">
        <h3>{{ t('cfgReview') }}</h3>
        <p>{{ t('cfgRevision') }} {{ plan.expected_revision }} · {{ t('cfgExpires') }} {{ formatTime(plan.expires_ms) }}</p>
        <code class="hash">{{ plan.plan_hash }}</code>
        <div class="table-wrap"><table><thead><tr><th>{{ t('cfgField') }}</th><th>{{ t('cfgLayerBefore') }}</th><th>{{ t('cfgLayerAfter') }}</th><th>{{ t('cfgEffectiveBefore') }}</th><th>{{ t('cfgEffectiveAfter') }}</th><th>{{ t('cfgOrigin') }}</th></tr></thead><tbody><tr v-for="row in plan.diff" :key="row.path"><td><code>{{ row.path }}</code><br />{{ row.action }}</td><td>{{ layerDisplay(row.layer_before, row.layer_before_null) }}</td><td>{{ layerDisplay(row.layer_after, row.layer_after_null) }}</td><td>{{ display(row.before) }}</td><td>{{ display(row.after) }}</td><td>{{ row.origin }}</td></tr></tbody></table></div>
        <p v-if="plan.blocked.length" role="alert">{{ t('cfgBlocked') }} <code>{{ JSON.stringify(plan.blocked) }}</code></p>
        <p v-if="plan.expires_ms <= now" role="status">{{ t('cfgExpired') }}</p>
        <button class="btn btn--primary" type="button" :disabled="!canCommit" @click="send">{{ t('cfgCommit') }}</button>
      </section>
      <section v-if="unknownOutcome" class="cfg-section" role="status"><p>{{ t('cfgUnknown') }}</p><code>{{ submission?.application_id }}</code><div class="actions"><button class="btn" type="button" :disabled="busy" @click="readSubmission">{{ t('cfgReadResult') }}</button><button class="btn" type="button" :disabled="busy" @click="send">{{ t('cfgRetryCommit') }}</button></div></section>
      <section v-if="result || resultError" class="cfg-section">
        <h3>{{ t('cfgResult') }}</h3>
        <ErrorNotice v-if="resultError" :error="resultError" :stale="!!result" :at="resultReadAt" @retry="submission ? readSubmission() : result ? showApplication(result.application_id) : state.load()" />
        <template v-if="result"><p><code>{{ result.application_id }}</code> · {{ t('cfgStatus') }}: <strong>{{ result.state }}</strong> · {{ t('cfgRevision') }} {{ result.config_revision }}</p><p v-if="result.state === 'saved_pending'">{{ t('cfgPending') }}</p><ul><li v-for="block in result.blocks" :key="block.name"><code>{{ block.name }}</code>: {{ block.state }}</li></ul></template>
        <template v-if="result">
          <h4>{{ t('cfgReadback') }}</h4><p>{{ t('cfgReadbackScope') }}</p>
          <button class="btn" type="button" :disabled="locked" @click="refreshReadback">{{ t('cfgReadbackRefresh') }}</button>
          <ErrorNotice v-if="readbackError" :error="readbackError" :stale="!!readback" :at="readback?.checked_ms" @retry="refreshReadback" />
          <template v-if="readback && readback.initial_receipt.application_id === result.application_id">
            <p>{{ t('cfgStatus') }}: {{ readerState(readback.state) }} · {{ t('cfgRevision') }} {{ readback.observed_revision }} · {{ formatTime(readback.checked_ms) }}</p>
            <p v-for="publication in publications" :key="publication.block">{{ publicationLabel(publication.block) }}: {{ readerState(publication.state) }}<template v-if="publication.error"> · <code>{{ publication.error }}</code></template></p>
            <ul><li v-for="field in readback.fields" :key="field.path"><code>{{ field.path }}</code>: {{ readerState(field.state) }}</li></ul>
            <button class="btn" type="button" :disabled="locked || !ready || dirty || !!readbackError || readback.state === 'superseded' || readback.observed_revision !== state.data.value.config_revision" @click="rollbackPreview">{{ t('cfgRollbackPreview') }}</button>
            <p>{{ t('cfgRollbackScope') }}</p>
          </template>
        </template>
      </section>
      <section class="cfg-section"><h3>{{ t('cfgHistory') }}</h3><ul><li v-for="application in applications" :key="application.application_id"><button class="btn" type="button" :disabled="locked" @click="showApplication(application.application_id)">{{ t('cfgDetails') }}</button> <code>{{ application.application_id }}</code> · {{ application.state }} · {{ application.change_reason }}</li></ul></section>
      <section class="cfg-section">
        <h3>{{ t('cfgExportTitle') }}</h3>
        <p class="muted">{{ t('cfgExportHint') }}</p>
        <div class="actions">
          <button class="btn" type="button" :disabled="exportBusy || locked" @click="runExport('v2')">{{ t('cfgExportV2') }}</button>
          <button class="btn" type="button" :disabled="exportBusy || locked" @click="runExport('v1')">{{ t('cfgExportV1') }}</button>
        </div>
        <template v-if="exported">
          <p><code>{{ exported.filename }}</code> · {{ t('cfgExportExcluded') }}: <code>{{ exported.excluded.join(', ') }}</code></p>
          <template v-if="exported.loss_report?.length">
            <p><strong>{{ t('cfgExportLossReport') }}</strong></p>
            <ul><li v-for="item in exported.loss_report" :key="item.domain"><code>{{ item.domain }}</code> ({{ item.status }}): {{ item.detail }}</li></ul>
          </template>
          <button class="btn" type="button" @click="downloadExport">{{ t('cfgExportDownload') }}</button>
        </template>
      </section>
      <details class="cfg-section">
        <summary>{{ t('cfgRegistryTitle') }} ({{ allFields.length }})</summary>
        <ul><li v-for="family in protectedFamilies" :key="family"><code>{{ family }}</code></li></ul>
        <div class="registry-controls">
          <input v-model="registryQuery" type="search" :placeholder="t('cfgRegistrySearch')" />
          <select v-model="registryScope">
            <option value="all">{{ t('cfgRegistryAllScopes') }}</option>
            <option value="family_or_node">family_or_node</option>
            <option value="node">node</option>
            <option value="unit">unit</option>
            <option value="boot">boot</option>
            <option value="data">data</option>
            <option value="runtime">runtime</option>
            <option value="secret">secret</option>
          </select>
        </div>
        <div class="table-wrap registry-list">
          <table>
            <thead><tr><th>{{ t('cfgField') }}</th><th>{{ t('cfgRegistryType') }}</th><th>{{ t('cfgRegistryScope') }}</th><th>{{ t('cfgRegistryBounds') }}</th><th>{{ t('cfgRegistryClear') }}</th><th>{{ t('cfgStatus') }}</th></tr></thead>
            <tbody>
              <tr v-for="field in filteredRegistry" :key="field.path">
                <td><code>{{ field.path }}</code><br /><span class="muted">{{ field.unit }} · {{ field.validator }}</span></td>
                <td><code>{{ formatType(field) }}</code></td>
                <td><code>{{ field.scope }}</code></td>
                <td><code>{{ formatBounds(field) }}</code></td>
                <td><code>{{ field.clear_behavior }}</code></td>
                <td>{{ field.supported ? t('cfgRegistryEditable') : t('cfgRegistryReadOnly') }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </details>
      <p class="muted">{{ t('cfgUnavailable') }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.cfg-form { border: 0; padding: 0; margin: 0; min-width: 0; display: flex; flex-direction: column; gap: var(--sp-3); }
.cfg-form > label, .field-row > label { display: flex; flex-direction: column; gap: var(--sp-1); }
.field-row > .muted { overflow-wrap: anywhere; }
.cfg-section, .field-row { padding: var(--sp-3); border: 1px solid var(--line); border-radius: var(--r-sm); display: flex; flex-direction: column; gap: var(--sp-2); }
.choose { display: flex !important; flex-direction: row !important; align-items: center; gap: var(--sp-2); flex-wrap: wrap; }
input:not([type="checkbox"]), textarea, select { width: 100%; box-sizing: border-box; color: var(--ink); background: var(--surface); border: 1px solid var(--line); border-radius: var(--r-sm); font: inherit; padding: var(--sp-2); }
textarea { resize: vertical; }
.actions, .registry-controls { display: flex; gap: var(--sp-2); flex-wrap: wrap; }
.registry-controls > * { flex: 1 1 12rem; }
.registry-list { max-height: 24rem; overflow: auto; }
input:disabled, textarea:disabled, select:disabled { opacity: .6; }
code, td { overflow-wrap: anywhere; }
.hash { word-break: break-all; }
.identity, .muted { color: var(--muted); font-size: var(--fs-sm); }
.table-wrap { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; font-size: var(--fs-sm); }
th, td { text-align: left; vertical-align: top; border-bottom: 1px solid var(--line); padding: var(--sp-2); min-width: 8rem; }
.actions { display: flex; flex-wrap: wrap; gap: var(--sp-2); }
.btn { border: 1px solid var(--line); background: var(--surface); color: var(--ink); border-radius: var(--r-sm); padding: var(--sp-2); cursor: pointer; }
.btn--primary { background: var(--accent); color: var(--on-accent); }
.btn:disabled { opacity: .5; cursor: not-allowed; }
ul { padding-left: var(--sp-4); } li { margin-block: var(--sp-1); }
</style>
