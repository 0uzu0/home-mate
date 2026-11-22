import assert from 'node:assert/strict'
import { createRenderer, nextTick } from 'vue'
import { createRouter, createMemoryHistory } from 'vue-router'
import AgentPersonaCard from '../src/components/settings/AgentPersonaCard.vue'
import WorkingMemoryCard from '../src/components/settings/WorkingMemoryCard.vue'
import FleetCard from '../src/components/settings/FleetCard.vue'
import FleetRolloutCard from '../src/components/config/FleetRolloutCard.vue'
import DeviceFreshnessNotice from '../src/components/DeviceFreshnessNotice.vue'
import OpenSocketsCard from '../src/components/tools/OpenSocketsCard.vue'
import TracesCard from '../src/components/diagnostics/TracesCard.vue'
import FollowUpPanel from '../src/components/interaction/FollowUpPanel.vue'
import StoryJokeCard from '../src/components/tools/StoryJokeCard.vue'
import LearningCard from '../src/components/tools/LearningCard.vue'
import AcousticsCard from '../src/components/device/AcousticsCard.vue'
import HomeCard from '../src/components/tools/HomeCard.vue'
import SettingsPage from '../src/views/SettingsPage.vue'
import { setAdvancedMode } from '../src/stores/advanced'
import * as devices from '../src/stores/devices'

// Render real SFC templates/setup/lifecycle with Vue's renderer. The host is
// in-memory; HTTP is intercepted at fetch, keeping actual API clients in use.
function node(tag = '', text = ''): any {
  return { tag, text, children: [], options: [], parent: null, props: {}, listeners: {}, tagName: tag.toUpperCase(),
    addEventListener(name: string, fn: unknown) { this.listeners[name] = fn },
    removeEventListener(name: string) { delete this.listeners[name] },
    setAttribute(name: string, value: unknown) { this.props[name] = value },
    removeAttribute(name: string) { delete this.props[name] },
    getRootNode() { return document },
    querySelectorAll() { return [] }, querySelector() { return null }, focus() {}, getClientRects() { return [1] },
  }
}
const teleportTarget = node('body')
const renderer = createRenderer<any, any>({
  querySelector: () => teleportTarget,
  createElement: tag => node(tag), createText: text => node('#text', text), createComment: text => node('#comment', text),
  insert(el, parent, anchor) { el.parent = parent; const at = parent.children.indexOf(anchor); if (at < 0) parent.children.push(el); else parent.children.splice(at, 0, el) },
  remove(el) { if (el.parent) el.parent.children.splice(el.parent.children.indexOf(el), 1) },
  setText(el, text) { el.text = text }, setElementText(el, text) { el.text = text; el.children = [] },
  parentNode: el => el.parent, nextSibling: el => el.parent?.children[el.parent.children.indexOf(el) + 1] ?? null,
  insertStaticContent: (c) => { const n = node('#static', c); return [n, n] },
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
function persona(content: string, version = 3) { return { content, version, updated_ms: 100, revisions: [{ id: version, version, content, author: 'console', note: '', created_ms: 100 }] } }
const user = { id: 'u1', person_id: 'p1', key: 'language', content: 'English', rationale: 'Selected', status: 'active', author: 'console', created_ms: 100, updated_ms: 100 }

// PP-09: the actual socket editor/client must retain rejected drafts and use
// fresh GET readback after a successful retry, never the submitted candidate.
{
  calls = []; let reject = true; let saved = false; let failReadback = false
  const current = { name: 'fixture.saved', service: 'fixture', description: 'Server-normalized description',
    parameters: { type: 'object' }, method: 'GET', effect: 'Query', endpoint_url: 'http://127.0.0.1:9/saved',
    timeout_ms: 500, enabled: true, auth_header: '', token_set: false, availability: 'Ready' }
  handler = async (_path, method) => {
    if (method === 'GET') return failReadback
      ? { status: 400, body: { error: 'Saved state readback unavailable' } }
      : { sockets: saved ? [current] : [] }
    if (reject) return { status: 400, body: { error: 'duplicate canonical tool name fixture.saved' } }
    saved = true
    return { ...current, description: 'Mutation response only' }
  }
  const { root, app } = mount(OpenSocketsCard); await flush()
  await button(root, 'Add socket').props.onClick(); await nextTick()
  edit(input(root, 'Tool name'), 'fixture.saved')
  edit(input(root, 'Description'), 'Draft description')
  edit(input(root, 'Endpoint URL'), 'http://127.0.0.1:9/saved'); await nextTick()
  await button(root, 'Save').props.onClick(); await flush()
  assert.ok(text(root).includes('duplicate canonical tool name fixture.saved'))
  assert.equal(input(root, 'Description').value, 'Draft description')
  assert.ok(!saved && !text(root).includes('Socket "fixture.saved" saved'))
  for (const poll of (globalThis as any).__polls.values()) await poll(); await flush()
  assert.equal(input(root, 'Description').value, 'Draft description')
  reject = false; failReadback = true
  await button(root, 'Save').props.onClick(); await flush()
  assert.ok(text(root).includes('Saved state readback unavailable'))
  assert.ok(!text(root).includes('Socket "fixture.saved" saved'))
  assert.equal(input(root, 'Description').value, 'Draft description')
  failReadback = false
  const readback = deferred()
  handler = async (_path, method) => method === 'GET' ? readback.promise : current
  const saving = button(root, 'Save').props.onClick(); await flush()
  const getsBeforePoll = calls.filter(c => c.method === 'GET').length
  for (const poll of (globalThis as any).__polls.values()) await poll(); await flush()
  assert.equal(calls.filter(c => c.method === 'GET').length, getsBeforePoll, 'poll cannot supersede save readback')
  assert.equal(input(root, 'Description').value, 'Draft description')
  assert.ok(!text(root).includes('Socket "fixture.saved" saved'))
  readback.resolve({ sockets: [current] }); await saving; await flush()
  assert.ok(text(root).includes('Server-normalized description'))
  assert.ok(!text(root).includes('Mutation response only'))
  assert.ok(!all(root).some(n => n.tag === 'textarea'), 'editor closes only after successful readback')
  assert.equal(calls.filter(c => c.method === 'POST').length, 3)
  assert.equal(calls.at(-1)?.method, 'GET')
  app.unmount()
}

// The same delete action can remove malformed persisted names without a URL
// path identity; this is repair, not a new settings-only reset mechanism.
{
  calls = []; let removed = false
  const broken = { name: '', service: 'fixture', description: 'Persisted malformed fixture',
    parameters: { type: 'object' }, method: 'GET', effect: 'Query', endpoint_url: 'http://127.0.0.1:9/fixture',
    timeout_ms: 500, enabled: true, auth_header: '', token_set: false, availability: 'Ready' }
  handler = async (path, method, body) => {
    if (method === 'GET') return { sockets: removed ? [] : [broken] }
    assert.equal(path, '/api/v0/config/sockets'); assert.equal(method, 'DELETE')
    assert.deepEqual(body, { name: '' }); removed = true
    return { removed: true, name: '' }
  }
  const { root, app } = mount(OpenSocketsCard); await flush()
  await button(root, 'Delete').props.onClick(); await nextTick()
  const confirms = all(teleportTarget).filter(n => n.tag === 'button' && text(n).trim() === 'Delete')
  await confirms.at(-1).props.onClick(); await flush()
  assert.ok(removed && !text(root).includes('Persisted malformed fixture'))
  app.unmount()
}

// UI-01: initial fetch is real, no empty overwrite before read, failed read
// remains locked; retry loads content/history/USER, save and rollback read back.
{
  calls = []; const initial = deferred(); let snap = persona('Existing persona')
  handler = async (path, method, body) => {
    if (path.includes('user-md')) return { entries: [user] }
    if (method === 'GET') return initial.promise
    assert.fail(`unexpected mutation before read: ${method} ${path}`)
  }
  const { root, app } = mount(AgentPersonaCard)
  assert.equal(button(root, 'Save Agent.md').props.disabled, true)
  await button(root, 'Save Agent.md').props.onClick()
  assert.equal(calls.filter(c => c.method !== 'GET').length, 0)
  initial.resolve({ status: 400, body: { error: 'read failed' } }); await flush()
  assert.equal(button(root, 'Save Agent.md').props.disabled, true)
  handler = async (path, method, body) => {
    if (path.includes('user-md')) return { entries: [user] }
    if (method === 'PUT') { snap = persona(body.content, 4); return snap }
    if (method === 'POST') { snap = persona('Existing persona', 5); return snap }
    return snap
  }
  await button(root, 'Retry').props.onClick(); await flush()
  const textarea = all(root).find(n => n.tag === 'textarea')
  assert.equal(textarea.value, 'Existing persona')
  assert.ok(text(root).includes('English') && text(root).includes('v3'))
  assert.equal(button(root, 'Save Agent.md').props.disabled, false)
  edit(textarea, 'New persona'); await nextTick()
  await button(root, 'Save Agent.md').props.onClick(); await flush()
  assert.equal(textarea.value, 'New persona'); assert.ok(text(root).includes('v4'))
  assert.equal(calls.filter(c => c.method === 'PUT').at(-1)?.body.content, 'New persona')
  // Add a historic row so the real rollback control can be exercised.
  snap.revisions.push(persona('Existing persona', 3).revisions[0])
  await button(root, 'Discard draft and reload').props.onClick(); await flush()
  const rollback = all(root).find(n => n.tag === 'button' && text(n).includes('Rollback') && !n.props.disabled)
  assert.ok(rollback, 'historic rollback button rendered'); await rollback.props.onClick(); await flush()
  assert.equal(textarea.value, 'Existing persona'); assert.ok(text(root).includes('v5'))
  app.unmount()
}

// UI-02: poll status/counters without replacing dirty URL/toggles/token;
// failed reload keeps draft, explicit successful discard and save read back.
{
  calls = []; let status = { enabled: false, upload_enabled: false, nas_base_url: 'http://old.example', token_set: false, backlog_count: 0 }; let fail = false
  handler = async (path, method, body) => {
    if (path.includes('working-memory')) return { items: [], count: 0 }
    if (method === 'PUT') { status = { ...status, ...body, token_set: !!body.token }; return { status, config: body } }
    return fail ? { status: 400, body: { error: 'read failed' } } : status
  }
  const { root, app } = mount(WorkingMemoryCard); await flush()
  const url = input(root, 'NAS memory-svc URL')
  const token = all(root).find(n => n.tag === 'input' && n.type === 'password')
  const checks = all(root).filter(n => n.tag === 'input' && n.type === 'checkbox')
  edit(url, 'http://new.example'); edit(checks[0], true); edit(checks[1], true); edit(token, 'draft-token'); await nextTick()
  status.backlog_count = 9
  for (const poll of (globalThis as any).__polls.values()) await poll(); await flush()
  assert.equal(url.value, 'http://new.example'); assert.equal(token.value, 'draft-token')
  assert.ok(text(root).includes('9'))
  fail = true; await button(root, 'Discard draft and reload').props.onClick(); await flush()
  assert.equal(url.value, 'http://new.example'); assert.equal(token.value, 'draft-token')
  assert.equal(button(root, 'Save sync settings').props.disabled, true)
  fail = false; await button(root, 'Discard draft and reload').props.onClick(); await flush()
  assert.equal(url.value, 'http://old.example'); assert.equal(token.value, '')
  edit(url, 'http://saved.example'); edit(checks[0], true); edit(checks[1], true); edit(token, 'new-token'); await nextTick()
  await button(root, 'Save sync settings').props.onClick(); await flush()
  const put = calls.find(c => c.method === 'PUT')!
  assert.equal(put.body.nas_base_url, 'http://saved.example'); assert.equal(put.body.enabled, true); assert.equal(put.body.upload_enabled, true); assert.equal(put.body.token, 'new-token')
  assert.equal(url.value, 'http://saved.example'); assert.equal(token.value, '')
  app.unmount()
}

// UI-03: first nonempty identity is clean; edits made while a refresh waits
// survive completion, and a clean field follows external identity changes.
{
  let fleet = { self_id: 'dot2-d', self_id_source: 'config', push_actor: 'fleet:dot2-d', nodes: [] }; let pending: ReturnType<typeof deferred> | null = null
  handler = async (path) => path.includes('/fleet') ? (pending ? pending.promise : fleet) : { vault: {} }
  const { root, app } = mount(FleetCard); await flush()
  const id = input(root, 'This node’s name')
  assert.equal(id.value, 'dot2-d'); assert.equal(button(root, 'Save').props.disabled, true)
  pending = deferred(); const refresh = button(root, 'Refresh').props.onClick(); await nextTick()
  edit(id, 'my-draft'); await nextTick(); fleet = { ...fleet, self_id: 'external-id' }; pending.resolve(fleet); pending = null
  await refresh; await flush(); assert.equal(id.value, 'my-draft')
  await button(root, 'Discard draft and reload').props.onClick(); await flush()
  assert.equal(id.value, 'external-id'); assert.equal(button(root, 'Save').props.disabled, true)
  fleet = { ...fleet, self_id: 'latest-id' }; await button(root, 'Refresh').props.onClick(); await flush()
  assert.equal(id.value, 'latest-id'); assert.equal(button(root, 'Save').props.disabled, true)
  app.unmount()
}

// UI-04: failed reads preserve data and its real time, partial success moves
// only that resource; notices use the corresponding success time.
{
  let now = 1000; const realNow = Date.now; Date.now = () => now
  let failed = new Set<string>()
  handler = async path => {
    if (failed.has(path)) return { status: 400, body: { error: `${path} unavailable` } }
    if (path.endsWith('/mode')) return { devices: [] }
    if (path.endsWith('/devices')) return { gateway: { devices: [] } }
    return { meta: { 'dot2-d': { name: 'D', room: 'Kids room' } } }
  }
  await devices.refresh()
  assert.equal(devices.modeUpdatedAt.value, 1000); assert.equal(devices.gatewayUpdatedAt.value, 1000); assert.equal(devices.metaUpdatedAt.value, 1000)
  const paths = calls.slice(-3).map(c => c.path); failed = new Set(paths); now = 2000
  const oldMode = devices.mode.value; await devices.refresh()
  assert.equal(devices.attemptedAt.value, 2000); assert.equal(devices.mode.value, oldMode)
  assert.equal(devices.modeUpdatedAt.value, 1000); assert.equal(devices.gatewayUpdatedAt.value, 1000); assert.equal(devices.metaUpdatedAt.value, 1000)
  failed = new Set(paths.filter(p => !p.endsWith('/devices'))); now = 3000; await devices.refresh()
  assert.equal(devices.modeUpdatedAt.value, 1000); assert.equal(devices.gatewayUpdatedAt.value, 3000); assert.equal(devices.metaUpdatedAt.value, 1000)
  const { root, app } = mount(DeviceFreshnessNotice); await nextTick()
  assert.ok(text(root).includes('Interaction state') && text(root).includes('Device labels'))
  assert.ok(!text(root).includes('Device state'))
  assert.equal(all(root).filter(n => n.props.class?.includes?.('error--stale')).length, 2)
  app.unmount(); Date.now = realNow
}

// #110: Household invitations, dot2-d-first preflight diff, stop-on-failure + skipped targets,
// 409 CONFIG_CONFLICT draft preservation, retry idempotency, active run resume, and per-unit rollback.
{
  calls = []
  let conflictOnCommit = true
  let runState: any = null
  const configState = {
    config_revision: 3, data_epoch: 1, core_version: '0.1.0', schema_version: 19,
    target: { household_id: 'hh-1', node_id: 'dot2-d', enrolled: true, node_id_source: 'enrollment' },
    layer_mode: 'partial', fields: [], stored: {}, resolved: {}, origins: {}, bindings: [],
    capabilities: { supported_blocks: ['mode'], rollback: true, fleet: true, enrollment: true, can_enroll: true },
    applications: [],
  }
  const fleetDoc = {
    self_id: 'dot2-d', self_id_source: 'config', push_actor: 'fleet:dot2-d',
    nodes: [
      { id: 'dot2-e', base_url: 'http://127.0.0.1:8931', token_set: true },
      { id: 'dot2-f', base_url: 'http://127.0.0.1:8932', token_set: true },
    ],
  }
  handler = async (path, method, body) => {
    if (path === '/api/v1/config/state') return configState
    if (path.endsWith('/fleet')) return fleetDoc
    if (path === '/api/v1/config/household/invitations' && method === 'POST') {
      assert.equal(body.expected_node_id, 'dot2-e')
      return { invitation_id: 'inv-1', household_id: 'hh-1', expected_node_id: 'dot2-e', token: 'tok-secret-1', membership_generation: 2, data_epoch: 1, created_ms: 1000, expires_ms: 601000 }
    }
    if (path === '/api/v1/config/household/join' && method === 'POST') {
      assert.equal(body.token, 'tok-join-1')
      assert.equal(body.issuer_node_id, 'dot2-d')
      return { household_id: 'hh-1', node_id: 'dot2-e', enrolled: true, invitation_id: 'inv-1', membership_generation: 2 }
    }
    if (path === '/api/v1/config/fleet/preflight' && method === 'POST') {
      assert.deepEqual(body.targets, ['dot2-d', 'dot2-e', 'dot2-f'], 'dot2-d must be ordered first')
      return {
        household_id: 'hh-1',
        ordered_targets: ['dot2-d', 'dot2-e', 'dot2-f'],
        all_ok: true,
        migration_report: {},
        package: body.package,
        targets: [
          { node_id: 'dot2-d', ok: true, plan_id: 'p-d', plan_hash: 'h-d', expected_revision: 3, diff: [{ path: 'mode.follow_window_enabled', before: false, after: true, origin: 'family_policy', action: 'set', layer_before: null, layer_after: true }], blocked: [] },
          { node_id: 'dot2-e', ok: true, plan_id: 'p-e', plan_hash: 'h-e', expected_revision: 1, diff: [{ path: 'mode.follow_window_enabled', before: false, after: true, origin: 'family_policy', action: 'set', layer_before: null, layer_after: true }], blocked: [] },
          { node_id: 'dot2-f', ok: true, plan_id: 'p-f', plan_hash: 'h-f', expected_revision: 1, diff: [{ path: 'mode.follow_window_enabled', before: false, after: true, origin: 'family_policy', action: 'set', layer_before: null, layer_after: true }], blocked: [] },
        ],
      }
    }
    if (path === '/api/v1/config/fleet/runs' && method === 'POST') {
      if (conflictOnCommit) return { status: 409, body: { error: 'CONFIG_CONFLICT', detail: 'CONFIG_CONFLICT: revision changed' } }
      runState = {
        run_id: body.run_id, household_id: 'hh-1', data_epoch: 1, package_hash: 'pkg-h', state: 'failed', created_ms: 1000, updated_ms: 2000,
        error: { error: 'PEER_OFFLINE', detail: 'dot2-e unreachable', status: 409 },
        targets: [
          { node_id: 'dot2-d', ord: 0, base_url: 'local', plan_id: 'p-d', plan_hash: 'h-d', expected_revision: 3, application_id: 'app-d', state: 'applied', result: {}, updated_ms: 1500 },
          { node_id: 'dot2-e', ord: 1, base_url: 'http://127.0.0.1:8931', plan_id: 'p-e', plan_hash: 'h-e', expected_revision: 1, application_id: 'app-e', state: 'failed', result: {}, updated_ms: 1800 },
          { node_id: 'dot2-f', ord: 2, base_url: 'http://127.0.0.1:8932', plan_id: 'p-f', plan_hash: 'h-f', expected_revision: 1, application_id: 'app-f', state: 'skipped', result: {}, updated_ms: 1800 },
        ],
      }
      return runState
    }
    if (path.endsWith('/retry') && method === 'POST') {
      runState = {
        ...runState,
        state: 'applied',
        error: null,
        targets: runState.targets.map((item: any) => ({ ...item, state: 'applied' })),
      }
      return runState
    }
    if (path.includes('/api/v1/config/fleet/runs/') && method === 'GET') {
      return runState
    }
    if (path.endsWith('/targets/dot2-d/rollback-preflight') && method === 'POST') {
      if (!body.commit) {
        return { run_id: runState.run_id, node_id: 'dot2-d', source_application_id: 'app-d', committed: false, preflight: { plan_id: 'rb-plan-d', plan_hash: 'rb-h', expected_revision: 4, expires_ms: Date.now() + 60000, diff: [], migration_report: {}, blocked: [] } }
      }
      runState = {
        ...runState,
        targets: runState.targets.map((item: any) => item.node_id === 'dot2-d' ? { ...item, state: 'rolled_back' } : item),
      }
      return { run_id: runState.run_id, node_id: 'dot2-d', source_application_id: 'app-d', rollback_application_id: 'rb-app-d', committed: true, preflight: { plan_id: 'rb-plan-d', plan_hash: 'rb-h', expected_revision: 4, expires_ms: Date.now() + 60000, diff: [], migration_report: {}, blocked: [] } }
    }
    assert.fail(`unexpected ${method} ${path}`)
  }

  const { root, app } = mount(FleetRolloutCard); await flush()
  // Issue invitation
  edit(input(root, 'Expected node ID'), 'dot2-e'); await nextTick()
  await button(root, 'Issue invitation').props.onClick(); await flush()
  assert.ok(text(root).includes('tok-secret-1'))

  // Join invitation with payload preview
  const joinBox = all(root).find(n => n.tag === 'textarea' && String(n.props.placeholder || '').includes('token'))
  edit(joinBox, JSON.stringify({ token: 'tok-join-1', issuer_node_id: 'dot2-d', expected_node_id: 'dot2-e', household_id: 'hh-1', expires_ms: 9999999999999 }))
  await nextTick()
  assert.ok(text(root).includes('Invitation preview'))
  await button(root, 'Join existing household').props.onClick(); await flush()
  assert.ok(text(root).includes('Joined household hh-1'))

  // Select peers in reverse order (dot2-e, dot2-f, then dot2-d) -> verify dot2-d first
  for (const id of ['dot2-e', 'dot2-f', 'dot2-d']) {
    const cb = all(root).find(n => n.tag === 'input' && n.props.type === 'checkbox' && text(n.parent).trim() === id)
    cb.props.onChange({ target: { checked: true } })
  }
  await nextTick()
  assert.ok(text(root).includes('dot2-d → dot2-e → dot2-f'))

  // Preflight fleet
  await button(root, 'Preflight fleet').props.onClick(); await flush()
  assert.ok(text(root).includes('Target preflight diff (dot2-d)'))
  assert.ok(text(root).includes('mode.follow_window_enabled'))

  // 409 CONFIG_CONFLICT preserves draft
  const pkgTextarea = all(root).find(n => n.tag === 'label' && text(n).includes('Family policy package')).children.find((c: any) => c.tag === 'textarea')
  const draftBeforeConflict = pkgTextarea.value
  await button(root, 'Commit sequential rollout').props.onClick(); await flush()
  assert.ok(text(root).includes('409 CONFIG_CONFLICT'))
  assert.equal(pkgTextarea.value, draftBeforeConflict)

  // Commit sequential rollout -> stop on failure + skipped
  conflictOnCommit = false
  await button(root, 'Commit sequential rollout').props.onClick(); await flush()
  assert.ok(text(root).includes('Fleet rollout stopped on failure'))
  assert.ok(text(root).includes('applied_verified'))
  assert.ok(text(root).includes('failed'))
  assert.ok(text(root).includes('skipped'))

  // Resume run by ID
  await button(root, 'Load run status').props.onClick(); await flush()
  assert.equal(calls.filter(c => c.method === 'GET' && c.path.includes('/api/v1/config/fleet/runs/')).length, 1)

  // Retry failed & skipped targets
  await button(root, 'Retry failed & skipped targets').props.onClick(); await flush()
  assert.ok(!text(root).includes('Fleet rollout stopped on failure'))

  // Per-target rollback flow
  await button(root, 'Preview rollback').props.onClick(); await flush()
  assert.ok(text(root).includes('rb-plan-d'))
  await button(root, 'Commit rollback').props.onClick(); await flush()
  assert.ok(text(root).includes('Rolled back dot2-d (rb-app-d)'))
  assert.ok(text(root).includes('rolled_back'))

  app.unmount()
}

// ── UI-05: TracesCard wake attribution & FollowUpPanel silent close save ──
{
  let lastModePut: Record<string, unknown> | null = null
  const modeState = {
    default_duplex: 'half',
    barge_in: false,
    barge_protect_ms: 800,
    asr_wake_fallback: true,
    follow_window_enabled: true,
    follow_window_ms: 9000,
    context_window_ms: 240000,
    followup_credits: null,
    follow_window_tone: false,
    retreat_intent: true,
    retreat_words: ['退下吧'],
    retreat_ack: '拜拜啦',
    window_close_ack: '',
    spoken: { max_sentences: 2, max_chars: 120, detail_max_sentences: 6, detail_max_chars: 300, backstop_max_sentences: 400, backstop_max_chars: 2000, continue_hint: '', resume_lead: '' },
    devices: [{
      device: 'dot2-d',
      duplex: { capability: 'half', requested: 'half', negotiated: 'half', effective: 'half' },
      activity: 'idle',
      assistant_state: 'idle',
      current_task: null,
      input_window_open: false,
      follow_window_tone: false,
    }],
  }
  const traceRows = [
    { id: 'R1', session_id: 'S1', task_id: 'T1', source: 'device:asr', wake_channel: 'kws', wake_word: '你好小智', input_text: '今天天气', status: 'completed', created_ms: 1795000000000, duration_ms: 320 },
    { id: 'R2', session_id: 'S1', task_id: 'T2', source: 'device:asr', wake_channel: 'followup', wake_word: null, input_text: '明天呢', status: 'completed', created_ms: 1795000005000, duration_ms: 210 },
  ]
  handler = (path, method, body) => {
    const clean = path.split('?')[0]
    if (clean === '/api/v0/trace') return { traces: traceRows }
    if (clean === '/api/v0/trace/R1') {
      return {
        ...traceRows[0],
        goal: '晴',
        events: [{ seq: 1, kind: 'turn_start', payload: { source: 'device:asr', wake_channel: 'kws', wake_word: '你好小智', text: '今天天气' }, ts_ms: 1795000000000 }],
      }
    }
    if (clean === '/api/v0/messages') return { messages: [] }
    if (clean === '/api/v0/mode/dot2-d' && method === 'PUT') {
      lastModePut = body
      Object.assign(modeState, body)
      return { ok: true }
    }
    if (clean === '/api/v0/mode') return modeState
    return { status: 404, body: { error: 'not found' } }
  }

  const { root, app: tracesApp } = mount(TracesCard)
  await flush()
  assert.ok(text(root).includes('KWS · 你好小智'))
  assert.ok(text(root).includes('Follow-up'))
  const rowBtn = all(root).find(n => n.tag === 'button' && text(n).includes('今天天气'))
  assert.ok(rowBtn)
  await rowBtn.props.onClick(); await flush()
  assert.ok(text(root).includes('Wake: KWS · 你好小智'))
  tracesApp.unmount()

  const fpRoot = node('root')
  const fpApp = renderer.createApp(FollowUpPanel, {
    mode: modeState as any,
    device: modeState.devices[0] as any,
    refresh: async () => {},
  })
  fpApp.mount(fpRoot)
  await flush()
  const saveBtn = button(fpRoot, 'Save conversation window')
  assert.equal(saveBtn.props.disabled, false)
  await saveBtn.props.onClick(); await flush()
  assert.ok(lastModePut)
  assert.equal('window_close_ack' in lastModePut!, false)
  assert.equal(lastModePut!.context_window_ms, 240000)
  fpApp.unmount()
}

// ── UI-06: StoryJokeCard load, save and test connection ─────────────────
{
  let storyPut: any = null
  let jokePut: any = null
  let radioPut: any = null
  handler = (path, method, body) => {
    const clean = path.split('?')[0]
    if (clean === '/api/v0/config/services/story' && method === 'GET') {
      return { enabled: true, url: 'http://192.168.1.10:18083', timeout_ms: 4000, bearer_token_set: true, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/joke' && method === 'GET') {
      return { enabled: true, url: 'http://192.168.1.10:18081', timeout_ms: 4000, bearer_token_set: false, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/radio' && method === 'GET') {
      return { enabled: true, url: 'http://192.168.1.10:18086', timeout_ms: 4000, bearer_token_set: true, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/story' && method === 'PUT') {
      storyPut = body
      return { ...body, timeout_ms: 4000, bearer_token_set: true, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/joke' && method === 'PUT') {
      jokePut = body
      return { ...body, timeout_ms: 4000, bearer_token_set: false, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/radio' && method === 'PUT') {
      radioPut = body
      return { ...body, timeout_ms: 4000, bearer_token_set: true, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/story/test' && method === 'POST') return { ok: true, state: 'ready' }
    if (clean === '/api/v0/config/services/joke/test' && method === 'POST') return { ok: true, state: 'ready' }
    if (clean === '/api/v0/config/services/radio/test' && method === 'POST') return { ok: true, state: 'ready' }
    return { status: 404, body: { error: 'not found' } }
  }
  const { root, app } = mount(StoryJokeCard)
  await flush()
  await button(root, 'Save services').props.onClick(); await flush()
  assert.equal(storyPut?.url, 'http://192.168.1.10:18083')
  assert.equal(jokePut?.url, 'http://192.168.1.10:18081')
  assert.equal(radioPut?.url, 'http://192.168.1.10:18086')
  await button(root, 'Test connection').props.onClick(); await flush()
  assert.ok(text(root).includes('story: ok · joke: ok · radio: ok'))
  app.unmount()
}

// ── UI-07: LearningCard load, save and test connection ──────────────────
{
  let lrnPut: any = null
  handler = (path, method, body) => {
    const clean = path.split('?')[0]
    if (clean === '/api/v0/config/services/learning' && method === 'GET') {
      return { enabled: true, url: 'http://192.168.1.10:18085', timeout_ms: 4000, bearer_token_set: true, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/learning' && method === 'PUT') {
      lrnPut = body
      return { ...body, timeout_ms: 4000, bearer_token_set: true, state: 'ready' }
    }
    if (clean === '/api/v0/config/services/learning/test' && method === 'POST') return { ok: true, state: 'ready' }
    return { status: 404, body: { error: 'not found' } }
  }
  const { root, app } = mount(LearningCard)
  await flush()
  await button(root, 'Save learning config').props.onClick(); await flush()
  assert.equal(lrnPut?.url, 'http://192.168.1.10:18085')
  await button(root, 'Test learning service').props.onClick(); await flush()
  assert.ok(text(root).includes('configured'))
  assert.ok(text(root).includes('Open phone learning (/learn)'))
  app.unmount()
}

// ── UI-08: AcousticsCard optional onset_threshold_dbfs & honest receipts ─
{
  let putBody: any = null
  let putMode: 'applied' | 'failed' = 'applied'
  handler = (path, method, body) => {
    const clean = path.split('?')[0]
    if (clean === '/api/v0/devices/dot2-d/profile' && method === 'GET') {
      return {
        device: 'dot2-d',
        acoustic: {
          volume_cap: 0.85,
          mix_gain: 1.0,
          vad_threshold: 0.5,
          mic_gain_offset_db: 1.25,
          mic_calibrated: true,
        },
        led: {
          night_enabled: false,
          night_start_minute: 1320,
          night_end_minute: 420,
          night_brightness_pct: 20,
          orientation_rotation: 0,
          orientation_reverse: false,
        },
        acoustics_state: 'saved_pending',
        acoustics_receipt: null,
      }
    }
    if (clean === '/api/v0/devices/dot2-d/profile' && method === 'PUT') {
      putBody = body
      if (putMode === 'applied') {
        return {
          state: 'applied',
          device: 'dot2-d',
          acoustic: { ...body.acoustic },
          acoustics_state: 'applied',
          acoustics_pushed: true,
          acoustics_receipt: { state: 'active', block_generation: 11 },
        }
      }
      return {
        state: 'failed',
        device: 'dot2-d',
        acoustic: { volume_cap: 0.85, mix_gain: 1.0, vad_threshold: 0.5, mic_gain_offset_db: 1.25, mic_calibrated: true },
        acoustics_state: 'failed',
        acoustics_pushed: false,
        acoustics_receipt: { state: 'failed', reason: 'HARDWARE_APPLY_FAILED' },
      }
    }
    return { status: 404, body: { error: 'not found' } }
  }
  const acRoot = node('root')
  const acApp = renderer.createApp(AcousticsCard, {
    device: { device: 'dot2-d', online: true } as any,
  })
  acApp.mount(acRoot)
  await flush()
  // Blank onset_threshold_dbfs must NOT be filled with vad_threshold (0.5).
  assert.ok(text(acRoot).includes('Saved, awaiting device receipt'))
  await button(acRoot, 'Save & apply acoustics').props.onClick(); await flush()
  assert.equal(putBody?.acoustic?.vad_threshold, 0.5)
  assert.equal('onset_threshold_dbfs' in (putBody?.acoustic ?? {}), false)
  assert.ok(text(acRoot).includes('Applied on gateway (gen 11)'))

  putMode = 'failed'
  await button(acRoot, 'Save & apply acoustics').props.onClick(); await flush()
  assert.ok(text(acRoot).includes('Apply failed: HARDWARE_APPLY_FAILED'))
  acApp.unmount()
}

// UI-09: HomeCard applied-state summary honesty + SettingsPage 4-tab/advancedMode switching.
{
  let haState: any = {
    backend: 'none',
    entity_source: 'api',
    state: 'unconfigured',
    availability: 'unconfigured',
    base_url: '',
    token_set: false,
    entities: [],
    timeout_ms: 4000,
    retries: 1,
    tools: ['home.set'],
    claim: 'Not connected to any smart-home backend.',
    detail: '',
  }
  handler = async (path, method) => {
    if (path === '/api/v0/config/services/home' && method === 'GET') return haState
    if (path === '/api/v0/status' && method === 'GET') return { version: '0.0.1', gateway: { devices: [] }, devices: [] }
    if (path === '/api/v0/auth/status' && method === 'GET') return { paired: true, token_prefix: 'hm1_****' }
    if (path === '/api/v0/config/vault' && method === 'GET') return { sealed: 0, plaintext: 0, undecryptable: 0 }
    if (path.startsWith('/api/v1/config/')) return { target: { enrolled: false, node_id: 'dot2-d' }, capabilities: { can_enroll: true }, blocks: [], fields: [], applications: [], items: [] }
    if (path === '/api/v0/fleet') return { self_id: 'dot2-d', self_id_source: 'config', nodes: [] }
    return { history: [], items: [], devices: [] }
  }

  // 1. HomeCard: editing draft.backend to home_assistant before saving must NOT claim Connected.
  const hRoot = node('root')
  const hApp = renderer.createApp(HomeCard)
  hApp.mount(hRoot)
  await flush()
  assert.ok(text(hRoot).includes('Smart home not connected'))
  const sel = all(hRoot).find((n) => n.tag === 'select')
  assert.ok(sel, 'backend select exists')
  sel.props.onChange?.({ target: { value: 'home_assistant' } })
  await nextTick()
  assert.ok(text(hRoot).includes('Smart home not connected'), 'unsaved draft must not falsely claim connected')
  assert.ok(!text(hRoot).includes('Connected to'))
  hApp.unmount()

  // 2. HomeCard: saved home_assistant with state=unconfigured must NOT claim Connected.
  haState = { ...haState, backend: 'home_assistant', state: 'unconfigured', claim: 'Needs token and entity' }
  const hRoot2 = node('root')
  const hApp2 = renderer.createApp(HomeCard)
  hApp2.mount(hRoot2)
  await flush()
  assert.ok(text(hRoot2).includes('Needs token and entity'))
  assert.ok(!text(hRoot2).includes('Connected to'))
  hApp2.unmount()

  // 3. HomeCard: saved home_assistant with state=ready shows honest Connected summary.
  haState = {
    ...haState,
    backend: 'home_assistant',
    state: 'ready',
    token_set: true,
    entities: [{ name: 'Desk', entity: 'light.desk' }],
  }
  const hRoot3 = node('root')
  const hApp3 = renderer.createApp(HomeCard)
  hApp3.mount(hRoot3)
  await flush()
  assert.ok(text(hRoot3).includes('Connected to Home Assistant · 1 device(s) enabled'))
  hApp3.unmount()

  // 4. SettingsPage: 4-tab switching and advancedMode auto-switch.
  ;(globalThis.document as any).scripts ??= []
  setAdvancedMode(false)
  const sRoot = node('root')
  const sApp = renderer.createApp(SettingsPage)
  sApp.use(createRouter({ history: createMemoryHistory('/'), routes: [{ path: '/:p(.*)*', component: {} }] }))
  sApp.mount(sRoot)
  await flush()
  const tabBtns = () => all(sRoot).filter((n) => n.tag === 'button' && n.props.role === 'tab')
  assert.equal(tabBtns().length, 3, '3 tabs when advancedMode is off')
  setAdvancedMode(true)
  await flush()
  assert.equal(tabBtns().length, 4, '4 tabs when advancedMode is on')
  const advTab = tabBtns().find((b) => text(b).includes('Advanced & Fleet'))
  assert.equal(advTab?.props['aria-selected'], true, 'auto-switches to Advanced & Fleet when enabled')
  setAdvancedMode(false)
  await flush()
  assert.equal(tabBtns().length, 3)
  sApp.unmount()

  // 5. SettingsPage: opening Settings while advancedMode is ALREADY on still defaults to General.
  setAdvancedMode(true)
  const sRoot2 = node('root')
  const sApp2 = renderer.createApp(SettingsPage)
  sApp2.use(createRouter({ history: createMemoryHistory('/'), routes: [{ path: '/:p(.*)*', component: {} }] }))
  sApp2.mount(sRoot2)
  await flush()
  const tabs2 = all(sRoot2).filter((n) => n.tag === 'button' && n.props.role === 'tab')
  assert.equal(tabs2.length, 4)
  const genTab = tabs2.find((b) => text(b).includes('General'))
  assert.equal(genTab?.props['aria-selected'], true, 'defaults to General even when advancedMode is already on')
  setAdvancedMode(false)
  sApp2.unmount()
}
console.log('UI-01..09: mounted Vue lifecycle, HTTP, drafts, readback, traces, follow-up, story/joke, learning, acoustics and UI/UX simplification checks passed')
