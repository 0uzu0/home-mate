import assert from 'node:assert/strict'
import { createRenderer, nextTick } from 'vue'
import ConfigPlanCard from '../src/components/config/ConfigPlanCard.vue'
import { locale } from '../src/i18n'
import enConfig from '../src/i18n/en/config'
import zhConfig from '../src/i18n/zh/config'
import { newConfigIdentity } from '../src/api/configV2'
// Render real SFC templates/setup/lifecycle with Vue's renderer. The host is
// in-memory; HTTP is intercepted at fetch, keeping actual API clients in use.
function node(tag = '', text = ''): any {
  return { get options() { return this.children.filter((child: any) => child.tag === 'option') }, tag, text, children: [], parent: null, props: {}, listeners: {}, tagName: tag.toUpperCase(),
    addEventListener(name: string, fn: unknown) { this.listeners[name] = fn },
    removeEventListener(name: string) { delete this.listeners[name] },
    setAttribute(name: string, value: unknown) { this.props[name] = value },
    removeAttribute(name: string) { delete this.props[name] },
    getRootNode() { return document },
  }
}
const renderer = createRenderer<any, any>({
  createElement: tag => node(tag), createText: text => node('#text', text), createComment: text => node('#comment', text),
  insert(el, parent, anchor) { el.parent = parent; const at = parent.children.indexOf(anchor); if (at < 0) parent.children.push(el); else parent.children.splice(at, 0, el) },
  remove(el) { if (el.parent) el.parent.children.splice(el.parent.children.indexOf(el), 1) },
  setText(el, text) { el.text = text }, setElementText(el, text) { el.text = text; el.children = [] },
  parentNode: el => el.parent, nextSibling: el => el.parent?.children[el.parent.children.indexOf(el) + 1] ?? null,
  patchProp(el, key, _old, value) { el.props[key] = value; if (key === 'type' || key === 'value') el[key] = value },
})
function all(root: any): any[] { return [root, ...root.children.flatMap(all)] }
function text(root: any): string { return root.tag === '#comment' ? '' : root.text + root.children.map(text).join('') }
function button(root: any, label: string): any {
  const found = all(root).find(n => n.tag === 'button' && text(n).trim() === label)
  assert.ok(found, `button ${label} must render`); return found
}
function input(root: any, label: string): any {
  const wrap = all(root).find(n => n.tag === 'label' && text(n).includes(label))
  const field = wrap && (all(wrap).find(n => n.tag === 'input') ?? all(root).find(n => n.tag === 'input' && n.props.id === wrap.props.for))
  assert.ok(field, `input ${label} must render`); return field
}
function edit(el: any, value: unknown) {
  el.props['onUpdate:modelValue']?.(value)
  el.props.onInput?.({ target: { value } })
  el.props.onChange?.({ target: { checked: value } })
}
async function flush() { for (let i = 0; i < 14; i++) { await Promise.resolve(); await nextTick() } }
function mount(component: any) { const root = node('root'); const app = renderer.createApp(component); app.mount(root); return { root, app } }
let calls: { path: string; method: string; body: any }[] = []
let handler: (path: string, method: string, body: any) => any
Object.assign(globalThis, { fetch: async (path: string, init: any = {}) => {
  const method = init.method ?? 'GET'; const body = init.body ? JSON.parse(init.body) : null
  calls.push({ path, method, body })
  const result = await handler(path, method, body)
  const envelope = typeof result.status === 'number' && Object.hasOwn(result, 'body')
  return new Response(JSON.stringify(envelope ? result.body : result), { status: envelope ? result.status : 200 })
} })
function deferred() { let resolve!: (value: any) => void; const promise = new Promise(r => resolve = r); return { promise, resolve } }
const realRandomUUID = crypto.randomUUID
Object.defineProperty(crypto, 'randomUUID', { configurable: true, value: undefined })
assert.match(newConfigIdentity(), /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
Object.defineProperty(crypto, 'randomUUID', { configurable: true, value: realRandomUUID })
const field = (path: string, kind = 'string', scope = 'family_or_node', supported = true) => ({ path, kind, scope, supported, purposes: supported ? scope === 'node' ? ['node_config'] : ['node_config', 'family_policy'] : [], unit: '', default_source: 'stored', validator: 'typed', secret_policy: 'exclude', apply: 'saved_pending', rollback: 'unsupported', clear_behavior: 'remove_layer_override' })
let snapshot = {
  config_revision: 7, data_epoch: 2, core_version: 'test', schema_version: 19,
  target: { household_id: '', node_id: 'dot2-d', enrolled: false, node_id_source: 'config' }, layer_mode: 'partial',
  fields: [field('services.llm.model'), field('services.llm.api_key_env', 'string', 'node'), field('shared_memory.enabled', 'boolean'), field('persona.agent_md.content'), field('weather.max_days', 'integer'), field('music.library', 'object'), field('music.online', 'object'), field('openapi.operations', 'array'), { ...field('mode.followup_credits', 'integer'), nullable: true }, field('unit_acoustic_profiles.dot2-d.volume_cap', 'number', 'unit', false)],
  stored: { 'services.llm.model': 'old-model', 'services.llm.api_key_env': 'LOCAL_KEY', 'shared_memory.enabled': false, 'persona.agent_md.content': 'hello' },
  resolved: { 'services.llm.model': 'old-model', 'services.llm.api_key_env': 'LOCAL_KEY', 'shared_memory.enabled': false, 'persona.agent_md.content': 'hello' },
  origins: { 'services.llm.model': 'node' }, bindings: [{ binding_id: 'llm.api_key', bound: false, scope: 'stored_target_binding_only', environment_binding: 'not_probed' }],
  capabilities: { supported_blocks: ['services', 'shared_memory', 'persona', 'weather', 'music', 'openapi', 'mode'], rollback: false, fleet: false, enrollment: true, can_enroll: false }, applications: [] as any[],
}
let failRead = true
let commitMode = 'conflict'
let activePlan: any
let application: any
let failRuntimeRead = false
let runtimeRead: any
let publicationState = 'saved_pending'
let servicePublicationState = 'saved_pending'
let rollbackCalls = 0
let profileFailure = false
handler = async (path, method, body) => {
  if (path.endsWith('/state')) return failRead ? { status: 400, body: { error: 'state unavailable' } } : snapshot
  if (path.endsWith('/enrollment')) { assert.deepEqual(body, {}); snapshot.target = { household_id: 'household-1', node_id: 'dot2-d', enrolled: true, node_id_source: 'enrolled' }; return snapshot.target }
  if (path.endsWith('/rollback-preflight')) {
    rollbackCalls++
    assert.deepEqual(body, { expected_revision: snapshot.config_revision })
    return { ...activePlan, plan_id: crypto.randomUUID(), diff: [{ ...activePlan.diff[0], before: 'edited-model', after: 'old-model', layer_before: 'edited-model', layer_after: 'old-model' }] }
  }
  if (path.endsWith('/readback')) {
    if (failRuntimeRead) return { status: 500, body: { error: 'runtime read failed' } }
    runtimeRead = { initial_receipt: application, observed_revision: snapshot.config_revision, data_epoch: snapshot.data_epoch, checked_ms: Date.now(), state: 'partial_readback', fields: [{ path: 'services.llm.model', state: 'core_readback_verified', value: 'edited-model', dependency: null }, { path: 'shared_memory.enabled', state: 'pending_dependency', value: null, dependency: '#107' }], remote_health: 'not_probed', publication_isolation: 'core_blocks', publications: [{ block: 'persona', state: publicationState, error: publicationState === 'failed' ? 'PERSONA_PUBLICATION_FAILED' : null }, { block: 'services', state: servicePublicationState, error: servicePublicationState === 'failed' ? 'SERVICES_PUBLICATION_FAILED' : null }], publication: { block: 'persona', state: publicationState, error: publicationState === 'failed' ? 'PERSONA_PUBLICATION_FAILED' : null, isolation: 'desired_until_worker' } }
    runtimeRead.publications.push({ block: 'weather', state: 'applied', error: null }, { block: 'home', state: 'applied', error: null }, { block: 'music', state: 'applied', error: null }, { block: 'openapi', state: 'applied', error: null }, { block: 'mode', state: 'applied', error: null }, { block: 'news', state: 'applied', error: null }, { block: 'search', state: 'applied', error: null })
    return runtimeRead
  }
  if (path.endsWith('/profiles/dot2-d-persona')) {
    assert.deepEqual(body, {})
    if (profileFailure) return { status: 409, body: { error: 'PROFILE_BASELINE_PENDING' } }
    activePlan = { plan_id: crypto.randomUUID(), plan_hash: 'profile-reviewed-hash', expected_revision: snapshot.config_revision, expires_ms: Date.now() + 600000, diff: [{ path: 'persona.agent_md.content', before: 'hello', after: 'DOT2D_PERSONA_TEMPLATE', origin: 'node', action: 'set', layer_before: 'hello', layer_after: 'DOT2D_PERSONA_TEMPLATE' }], migration_report: { scope: 'persona_only' }, blocked: [] }
    return { profile_id: 'luna-dot2d-persona-v1', filename: 'luna-dot2d-persona-v1.json', scope: 'persona_only', complete_luna_profile: false, target_assurance: { enrolled_core_node_id: 'dot2-d', physical_device_identity: 'not_verified' }, package: { format: 'home-mate-config', version: 2, target: { household_id: snapshot.target.household_id, node_id: 'dot2-d' }, content_hash: 'server-artifact-hash', blocks: [{ name: 'persona', schema_version: 1, operations: [{ action: 'set', path: 'persona.agent_md.content', value: 'DOT2D_PERSONA_TEMPLATE' }] }], bindings: [] }, preflight: activePlan, preserved: ['custom_wake', 'services', 'unit_acoustics'], wake_policy: { action: 'preserve', recommended_default: '你好小智', gateway_effective_state: 'not_verified' }, unavailable: [{ issue: 108, paths: ['mode.*'] }, { issue: 111, paths: ['complete_dot2d_luna_profile'] }, { issue: 112, paths: ['node.wake'] }, { issue: 113, paths: ['unit_acoustic_profiles.*'] }] }
  }
  if (path.endsWith('/profiles/dot2-d-family')) {
    assert.deepEqual(body, {})
    activePlan = { plan_id: crypto.randomUUID(), plan_hash: 'family-profile-reviewed-hash', expected_revision: snapshot.config_revision, expires_ms: Date.now() + 600000, diff: [{ path: 'persona.agent_md.content', before: 'hello', after: 'DOT2D_FAMILY_PERSONA', origin: 'node', action: 'set', layer_before: 'hello', layer_after: 'DOT2D_FAMILY_PERSONA' }], migration_report: { profile_id: 'luna-dot2d-family-v1', scope: 'dot2d_family' }, blocked: [] }
    return {
      profile_id: 'luna-dot2d-family-v1', filename: 'luna-dot2d-family-v1.json', scope: 'dot2d_family', complete_luna_profile: true,
      target_assurance: { enrolled_core_node_id: 'dot2-d', immutable_target_lock: 'dot2-d', physical_device_identity: 'not_verified' },
      package: { format: 'home-mate-config', version: 2, target: { household_id: snapshot.target.household_id, node_id: 'dot2-d' }, content_hash: 'family-artifact-hash', blocks: [{ name: 'persona', schema_version: 1, operations: [{ action: 'set', path: 'persona.agent_md.content', value: 'DOT2D_FAMILY_PERSONA' }] }], bindings: [] },
      preflight: activePlan,
      retained_unit_refs: { 'unit_acoustic_profiles.dot2-d': { 'unit_acoustic_profiles.dot2-d.volume_cap': 80 }, audio_eq: { 'audio_eq.preset': 'flat' }, services: { 'services.llm.model': 'old-model' }, bindings: snapshot.bindings },
      compatibility: { core_api_schema: 2, required_capabilities: ['config_apply_v2', 'config_block_led', 'config_block_duplex'], readback_requirements: { required_blocks: ['persona', 'mode', 'node', 'device_led_configs'], publication_state: 'applied' } },
      rollback_instructions: { preflight_endpoint: '/api/v1/config/applications/{application_id}/rollback-preflight', commit_endpoint: '/api/v1/config/applications', expected_revision: snapshot.config_revision },
      blockers: [{ path: 'privacy.idle_asr_enabled', reason: 'UNSUPPORTED_FIELD', detail: 'privacy.idle_asr_enabled is registered as unsupported in this slice' }],
    }
  }
  if (path.endsWith('/preflight')) {
    const pkg = body.package
    assert.equal(pkg.source.schema_version, 19)
    assert.equal(pkg.content_hash, undefined)
    activePlan = { plan_id: crypto.randomUUID(), plan_hash: 'server-plan-hash', expected_revision: snapshot.config_revision, expires_ms: Date.now() + 600000, diff: pkg.blocks.flatMap((b: any) => b.operations.map((op: any) => ({ path: op.path, before: snapshot.stored[op.path], after: pkg.purpose === 'family_policy' ? snapshot.stored[op.path] : op.value, origin: 'node', action: op.action, layer_before: null, layer_after: op.value, layer_after_null: op.action === 'set' && op.value === null }))), migration_report: [], blocked: [] }
    return activePlan
  }
  if (path.endsWith('/applications') && method === 'POST') {
    assert.deepEqual(Object.keys(body).sort(), ['application_id', 'expected_revision', 'plan_hash', 'plan_id'])
    if (commitMode === 'conflict') return { status: 409, body: { error: 'CONFIG_CONFLICT' } }
    if (commitMode === 'unknown') throw new Error('connection lost after commit')
    application = { application_id: body.application_id, plan_id: body.plan_id, plan_hash: body.plan_hash, config_revision: 8, data_epoch: 2, purpose: 'family_policy', target: snapshot.target, actor: 'token-id', change_reason: 'Test family change', created_ms: Date.now(), state: 'saved_pending', blocks: [{ name: 'services', state: 'saved_pending' }], diff: activePlan.diff, layer_mode: 'partial' }
    snapshot = { ...snapshot, config_revision: 8, applications: [application] }
    return application
  }
  if (path.includes('/applications/')) return application ?? { status: 404, body: { error: 'not found' } }
  assert.fail(`unexpected ${method} ${path}`)
}
const { root, app } = mount(ConfigPlanCard)
await flush()
assert.ok(!all(root).some(n => n.tag === 'button' && text(n).trim() === 'Preview target diff'))
assert.equal(calls.filter(call => call.method !== 'GET').length, 0)
failRead = false
await button(root, 'Retry').props.onClick(); await flush()
assert.ok(text(root).includes('Initialize this node’s configuration identity'))
assert.equal(button(root, 'Initialize this node’s configuration identity').props.disabled, true)
assert.ok(all(root).some(node => node.tag === 'a' && node.props.href === '#fleet-identity'))
await button(root, 'Initialize this node’s configuration identity').props.onClick(); await flush()
assert.equal(calls.filter(call => call.path.endsWith('/enrollment')).length, 0)
snapshot = { ...snapshot, capabilities: { ...snapshot.capabilities, can_enroll: true } }
await button(root, 'Refresh').props.onClick(); await flush()
assert.equal(button(root, 'Initialize this node’s configuration identity').props.disabled, false)
await button(root, 'Initialize this node’s configuration identity').props.onClick(); await flush()
assert.equal(button(root, 'Preview target diff').props.disabled, false)
assert.ok(text(root).includes('No stored binding') && text(root).includes('Environment binding not checked'))
assert.ok(text(root).includes('physical hardware identity is not verified') && text(root).includes('including custom phrases'))
await button(root, 'Review dot2-d persona package').props.onClick(); await flush()
assert.ok(text(root).includes('DOT2D_PERSONA_TEMPLATE') && text(root).includes('profile-reviewed-hash'))
assert.ok(text(root).includes('complete_dot2d_luna_profile') && text(root).includes('#111') && text(root).includes('#112') && text(root).includes('#113'))
let downloadedBlob: Blob | undefined
let downloadedName = ''
let revoked = false
const originalCreate = document.createElement
const originalObjectURL = URL.createObjectURL
const originalRevoke = URL.revokeObjectURL
;(document as any).createElement = (tag: string) => tag === 'a' ? { href: '', download: '', click() { downloadedName = this.download } } : originalCreate.call(document, tag)
URL.createObjectURL = (blob: any) => { downloadedBlob = blob; return 'blob:synthetic-profile' }
URL.revokeObjectURL = () => { revoked = true }
await button(root, 'Download persona-only JSON').props.onClick(); await flush()
assert.equal(downloadedName, 'luna-dot2d-persona-v1.json'); assert.equal(revoked, true)
const downloaded = JSON.parse(await downloadedBlob!.text())
assert.equal(downloaded.target.node_id, 'dot2-d'); assert.equal(downloaded.content_hash, 'server-artifact-hash')
assert.deepEqual(downloaded.blocks.map((block: any) => block.name), ['persona'])
assert.equal(downloaded.preflight, undefined); assert.equal(downloaded.unavailable, undefined)
;(document as any).createElement = originalCreate
URL.createObjectURL = originalObjectURL; URL.revokeObjectURL = originalRevoke
profileFailure = true; locale.value = 'zh'; await flush()
await button(root, '预览 dot2-d 人格配置包').props.onClick(); await flush()
assert.ok(text(root).includes('请先完成或回滚待发布的服务配置') && text(root).includes('不修改或验证 Gateway'))
profileFailure = false; locale.value = 'en'; await flush()

function choosePath(path: string) {
  const label = all(root).find(n => n.tag === 'label' && text(n).includes(path))
  const check = all(label).find(n => n.tag === 'input' && n.type === 'checkbox')
  check.props.onChange({ target: { checked: true } })
}
function desired(path: string) {
  const row = all(root).find(n => n.tag === 'div' && n.props.class === 'field-row' && text(n).includes(path))
  return all(row).find(n => n.tag === 'input' && n.type === 'text')
}
choosePath('services.llm.model'); choosePath('services.llm.api_key_env'); await nextTick()
edit(desired('services.llm.model'), 'new-model')
await nextTick()
assert.ok(!all(root).some(n => n.tag === 'button' && text(n).trim() === 'Download persona-only JSON'), 'editing clears the generated profile preview')
const generationCalls = calls.filter(call => call.path.endsWith('/profiles/dot2-d-persona')).length
assert.equal(button(root, 'Review dot2-d persona package').props.disabled, true)
await button(root, 'Review dot2-d persona package').props.onClick(); await flush()
assert.equal(calls.filter(call => call.path.endsWith('/profiles/dot2-d-persona')).length, generationCalls, 'dirty draft cannot be replaced by a profile')

const reasonInput = all(root).find(n => n.tag === 'input' && n.props.placeholder === 'Explain why these fields are changing')
edit(reasonInput, 'Test family change')
const scope = all(root).find(n => n.tag === 'select')
scope.props['onUpdate:modelValue']('family_policy'); await nextTick()
assert.ok(!all(root).some(n => n.tag === 'div' && n.props.class === 'field-row' && text(n).includes('services.llm.api_key_env')))
await button(root, 'Preview target diff').props.onClick(); await flush()
let previewBody = calls.filter(call => call.path.endsWith('/preflight')).at(-1)!.body.package
assert.equal(previewBody.purpose, 'family_policy'); assert.equal(previewBody.target.node_id, undefined)
assert.deepEqual(previewBody.blocks.flatMap((block: any) => block.operations.map((op: any) => op.path)), ['services.llm.model'])
assert.deepEqual(previewBody.bindings, [{ binding_id: 'llm.api_key', action: 'preserve' }])
assert.equal(previewBody.blocks[0].operations[0].value, 'new-model')
assert.ok(text(root).includes('old-model') && text(root).includes('new-model') && text(root).includes('server-plan-hash'))
const row = all(root).find(node => node.tag === 'tr' && text(node).includes('services.llm.model'))
assert.deepEqual(row.children.filter((child: any) => child.tag === 'td').map(text), ['services.llm.modelset', 'No override (inherit)', 'new-model', 'old-model', 'old-model', 'node'])
// External target identity, reset epoch and revision changes invalidate the
// review while keeping the draft; only a new target preflight may commit.
async function pollState() { for (const poll of (globalThis as any).__polls.values()) await poll(); await flush() }
for (const change of [
  () => { snapshot = { ...snapshot, target: { ...snapshot.target, node_id: 'new-node' } } },
  () => { snapshot = { ...snapshot, data_epoch: snapshot.data_epoch + 1 } },
  () => { snapshot = { ...snapshot, config_revision: snapshot.config_revision + 1 } },
]) {
  change(); await pollState()
  assert.ok(!all(root).some(node => node.tag === 'button' && text(node).trim() === 'Submit reviewed changes'))
  assert.equal(desired('services.llm.model').value, 'new-model')
  await button(root, 'Preview target diff').props.onClick(); await flush()
}

edit(desired('services.llm.model'), 'edited-model'); await nextTick()
assert.ok(!all(root).some(n => n.tag === 'button' && text(n).trim() === 'Submit reviewed changes'))
await button(root, 'Preview target diff').props.onClick(); await flush()
await button(root, 'Submit reviewed changes').props.onClick(); await flush()
assert.ok(text(root).includes('The revision changed'))
assert.equal(desired('services.llm.model').value, 'edited-model')
await button(root, 'Preview target diff').props.onClick(); await flush()
commitMode = 'unknown'
await button(root, 'Submit reviewed changes').props.onClick(); await flush()
assert.ok(text(root).includes('The submission outcome is unknown'))
const firstId = calls.filter(call => call.method === 'POST' && call.path.endsWith('/applications')).at(-1)!.body.application_id
commitMode = 'success'
await button(root, 'Retry the same submission').props.onClick(); await flush()
const secondId = calls.filter(call => call.method === 'POST' && call.path.endsWith('/applications')).at(-1)!.body.application_id
assert.equal(firstId, secondId)
assert.ok(text(root).includes('saved_pending') && text(root).includes('Initial receipt: saved'))
assert.ok(text(root).includes('Measured acoustics') && text(root).includes('Shared-memory supervision and fleet distribution are still pending'))
assert.ok(!text(root).includes('applied_verified'))
await button(root, 'Read record').props.onClick(); await flush()
assert.ok(text(root).includes(application.application_id))
assert.ok(text(root).includes('Current setting verified') && text(root).includes('Waiting for shared-memory runtime support'))
assert.ok(text(root).includes('does not test provider connectivity'))
assert.ok(text(root).includes('Waiting to publish; previous settings remain active'))
publicationState = 'failed'
await button(root, 'Read current runtime values').props.onClick(); await flush()
assert.ok(text(root).includes('Publication failed; previous settings remain active') && text(root).includes('PERSONA_PUBLICATION_FAILED'))
publicationState = 'applied'
await button(root, 'Read current runtime values').props.onClick(); await flush()
assert.ok(text(root).includes('Published for new operations'))
for (const label of ['Weather publication', 'Home publication', 'Music publication', 'OpenAPI publication', 'Voice policy publication', 'News publication', 'Search publication']) assert.ok(text(root).includes(label + ': Published for new operations'))
servicePublicationState = 'failed'
await button(root, 'Read current runtime values').props.onClick(); await flush()
assert.ok(text(root).includes('Service publication: Publication failed; previous settings remain active') && text(root).includes('SERVICES_PUBLICATION_FAILED'))
servicePublicationState = 'applied'
await button(root, 'Read current runtime values').props.onClick(); await flush()
assert.equal(button(root, 'Preview verified rollback').props.disabled, true, 'dirty draft blocks rollback preview')
await button(root, 'Discard draft and reload').props.onClick(); await flush()
assert.equal(button(root, 'Preview verified rollback').props.disabled, false)
failRuntimeRead = true
await button(root, 'Read current runtime values').props.onClick(); await flush()
assert.equal(button(root, 'Preview verified rollback').props.disabled, true, 'stale readback error blocks rollback')
assert.ok(text(root).includes('Current setting verified'), 'previous evidence remains visibly stale')
failRuntimeRead = false
await button(root, 'Read current runtime values').props.onClick(); await flush()
await button(root, 'Preview verified rollback').props.onClick(); await flush()
assert.equal(rollbackCalls, 1)
assert.ok(text(root).includes('old-model'))
assert.equal(calls.filter(call => call.method === 'POST' && call.path.endsWith('/applications')).length, 3, 'rollback preview cannot mutate')
choosePath('weather.max_days'); await nextTick()
const weatherRow = all(root).find(n => n.tag === 'div' && n.props.class === 'field-row' && text(n).includes('weather.max_days'))
const weatherDays = all(weatherRow).find(n => n.tag === 'input' && n.type === 'number')
edit(weatherDays, '4')
edit(reasonInput, 'Review integer weather value')
await button(root, 'Preview target diff').props.onClick(); await flush()
const weatherDraft = calls.filter(call => call.path.endsWith('/preflight')).at(-1)!.body.package
assert.equal(weatherDraft.blocks.find((b: any) => b.name === 'weather').operations[0].value, 4)
await button(root, 'Discard draft and reload').props.onClick(); await flush()
scope.props['onUpdate:modelValue']('node_config'); await nextTick()
choosePath('music.library'); choosePath('music.online'); await nextTick()
const musicRow = (path: string) => all(root).find(n => n.tag === 'div' && n.props.class === 'field-row' && text(n).includes(path))
const libraryRow = musicRow('music.library')
edit(input(libraryRow, 'Endpoint'), 'http://library.local:4533')
edit(input(libraryRow, 'User'), 'listener')
const onlineAction = all(musicRow('music.online')).find(n => n.tag === 'select')
onlineAction.props.onChange({ target: { value: 'disable' } })
edit(reasonInput, 'Switch the library and disable the online connection')
await button(root, 'Preview target diff').props.onClick(); await flush()
const musicDraft = calls.filter(call => call.path.endsWith('/preflight')).at(-1)!.body.package
assert.deepEqual(musicDraft.blocks, [{ name: 'music', schema_version: 1, operations: [
  { action: 'replace', path: 'music.library', value: { endpoint: 'http://library.local:4533', user: 'listener' } },
  { action: 'set', path: 'music.online', value: null },
] }])
const onlineReview = all(root).find(n => n.tag === 'tr' && text(n).includes('music.online') && text(n).includes('set'))
assert.equal(text(onlineReview.children.filter((n: any) => n.tag === 'td')[2]), 'null', 'explicit null has a different layer display from inheritance')
assert.equal(calls.filter(call => call.method === 'POST' && call.path.endsWith('/applications')).length, 3, 'a Music preview does not apply')
await button(root, 'Discard draft and reload').props.onClick(); await flush()
choosePath('openapi.operations'); await nextTick()
const catalogRow = all(root).find(n => n.tag === 'div' && n.props.class === 'field-row' && text(n).includes('openapi.operations'))
const catalog = all(catalogRow).find(n => n.tag === 'textarea')
edit(reasonInput, 'Review an OpenAPI catalogue')
const preflightCount = calls.filter(call => call.path.endsWith('/preflight')).length
edit(catalog, '{"id":"wrong-shape"}')
await button(root, 'Preview target diff').props.onClick(); await flush()
assert.equal(calls.filter(call => call.path.endsWith('/preflight')).length, preflightCount)
assert.ok(text(root).includes('Enter a JSON array of OpenAPI operations.'))
const operations = [{ id: 'ping', method: 'get', path: '/ping', params: [] }]
edit(catalog, JSON.stringify(operations))
await button(root, 'Preview target diff').props.onClick(); await flush()
const openApiDraft = calls.filter(call => call.path.endsWith('/preflight')).at(-1)!.body.package
assert.deepEqual(openApiDraft.blocks, [{ name: 'openapi', schema_version: 1, operations: [
  { action: 'replace', path: 'openapi.operations', value: operations },
] }])
await button(root, 'Discard draft and reload').props.onClick(); await flush()
choosePath('mode.followup_credits'); await nextTick()
const creditRow = all(root).find(n => n.tag === 'div' && n.props.class === 'field-row' && text(n).includes('mode.followup_credits'))
const creditAction = all(creditRow).find(n => n.tag === 'select')
assert.equal(creditAction.value, 'null')
edit(reasonInput, 'Review unlimited follow-up')
await button(root, 'Preview target diff').props.onClick(); await flush()
const voiceDraft = calls.filter(call => call.path.endsWith('/preflight')).at(-1)!.body.package
assert.deepEqual(voiceDraft.blocks, [{ name: 'mode', schema_version: 1, operations: [
  { action: 'set', path: 'mode.followup_credits', value: null },
] }])
locale.value = 'zh'; await flush()
assert.ok(text(root).includes('当前实际消费者读回') && text(root).includes('预览已验证值的回滚') && text(root).includes('已发布，新操作将使用新设置'))
for (const label of ['天气配置发布', '家庭自动化配置发布', '音乐配置发布', 'OpenAPI 配置发布', '语音策略发布', '新闻配置发布', '搜索配置发布']) assert.ok(text(root).includes(label))
assert.ok(text(root).includes('不限跟进轮数'))
locale.value = 'en'; await flush()

// #111: luna-dot2d-family-v1 review, retained_unit_refs, readback_requirements, rollback_instructions, blockers & download
snapshot = { ...snapshot, target: { ...snapshot.target, node_id: 'dot2-d' } }
await button(root, 'Discard draft and reload').props.onClick(); await flush()
await button(root, 'Review luna-dot2d-family-v1').props.onClick(); await flush()
assert.ok(text(root).includes('unit_acoustic_profiles.dot2-d.volume_cap'))
assert.ok(text(root).includes('device_led_configs'))
assert.ok(text(root).includes('UNSUPPORTED_FIELD'))
assert.ok(text(root).includes('privacy.idle_asr_enabled'))
assert.ok(text(root).includes('/api/v1/config/applications/{application_id}/rollback-preflight'))
const downloads: { download: string; content: string }[] = []
const realCreateElement = document.createElement
const realCreateObjectURL = URL.createObjectURL
const realRevokeObjectURL = URL.revokeObjectURL
let lastBlob: Blob | null = null
URL.createObjectURL = (blob: any) => { lastBlob = blob; return 'blob:test-family' }
URL.revokeObjectURL = () => {}
;(document as any).createElement = (tag: string) => tag === 'a' ? {
  tag,
  href: '',
  download: '',
  click() {
    downloads.push({ download: this.download, content: lastBlob ? 'ok' : '' })
  },
} : realCreateElement.call(document, tag)
await button(root, 'Download luna-dot2d-family-v1.json').props.onClick(); await flush()
assert.equal(downloads[0]?.download, 'luna-dot2d-family-v1.json')
assert.equal(downloads[0]?.content, 'ok')
URL.createObjectURL = realCreateObjectURL
URL.revokeObjectURL = realRevokeObjectURL
;(document as any).createElement = realCreateElement
assert.deepEqual(Object.keys(zhConfig).sort(), Object.keys(enConfig).sort(), 'zh and en config i18n keys must match 1:1')
app.unmount()
console.log('Config v2: mounted real API-client preview, enrollment, scope filtering, draft invalidation, CAS conflict, idempotent retry and saved-pending history checks passed')
