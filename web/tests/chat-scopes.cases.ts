import assert from 'node:assert/strict'
import { createRenderer, nextTick } from 'vue'
import ChatCard from '../src/components/diagnostics/ChatCard.vue'
import AgentPersonaCard from '../src/components/settings/AgentPersonaCard.vue'
import MemoryCard from '../src/components/settings/MemoryCard.vue'
import ConversationCard from '../src/components/device/ConversationCard.vue'
import FollowUpPanel from '../src/components/interaction/FollowUpPanel.vue'
import { authToken } from '../src/auth'
import { locale } from '../src/i18n'

function node(tag = '', text = ''): any {
  return { tag, text, children: [], parent: null, props: {}, tagName: tag.toUpperCase(),
    addEventListener() {}, removeEventListener() {}, setAttribute() {}, removeAttribute() {},
    getRootNode() { return document }, querySelector() { return null }, querySelectorAll() { return [] }, focus() {},
  }
}
const portal = node('body')
const renderer = createRenderer<any, any>({
  querySelector: () => portal,
  createElement: tag => node(tag), createText: text => node('#text', text), createComment: text => node('#comment', text),
  insert(el, parent, anchor) { el.parent = parent; const at = parent.children.indexOf(anchor); if (at < 0) parent.children.push(el); else parent.children.splice(at, 0, el) },
  remove(el) { if (el.parent) el.parent.children.splice(el.parent.children.indexOf(el), 1) },
  setText(el, text) { el.text = text }, setElementText(el, text) { el.text = text; el.children = [] },
  parentNode: el => el.parent, nextSibling: el => el.parent?.children[el.parent.children.indexOf(el) + 1] ?? null,
  patchProp(el, key, _old, value) { el.props[key] = value; if (key === 'type' || key === 'value') el[key] = value },
})
const all = (root: any): any[] => [root, ...root.children.flatMap(all)]
const text = (root: any): string => root.tag === '#comment' ? '' : root.text + root.children.map(text).join('')
function button(root: any, label: string): any {
  const result = all(root).find(n => n.tag === 'button' && text(n).trim() === label)
  assert.ok(result, `button ${label} must render`); return result
}
function field(root: any, label: string): any {
  const wrapper = all(root).find(n => n.tag === 'label' && text(n) === label)
  const result = wrapper && all(root).find(n => n.tag === 'input' && n.props.id === wrapper.props.for)
  assert.ok(result, `input ${label} must render`); return result
}
const composer = (root: any) => all(root).find(n => n.props.id === 'chat-input')
function edit(el: any, value: any) {
  el.props['onUpdate:modelValue']?.(value)
  el.props.onInput?.({ target: { value } })
}
async function choose(root: any, value: string) {
  all(root).find(n => n.tag === 'select').props.onChange({ target: { value } }); await nextTick()
}
async function flush() { for (let i = 0; i < 20; i++) { await Promise.resolve(); await nextTick() } }
function mount(component: any = ChatCard, props: any = {}) {
  const root = node('root'); const app = renderer.createApp(component, props); app.mount(root); return { root, app }
}
function deferred() { let resolve!: (v: any) => void; const promise = new Promise(r => { resolve = r }); return { promise, resolve } }
let calls: Array<{ path: string; method: string; body: any; headers: any }> = []
let handler: (path: string, method: string, body: any) => any
Object.assign(globalThis, { fetch: async (path: string, init: any = {}) => {
  const method = init.method ?? 'GET'; const body = init.body ? JSON.parse(init.body) : null
  calls.push({ path, method, body, headers: init.headers })
  const result = await handler(path, method, body)
  if (result instanceof Response) return result
  const envelope = typeof result.status === 'number' && Object.hasOwn(result, 'body')
  return new Response(JSON.stringify(envelope ? result.body : result), { status: envelope ? result.status : 200 })
} })
function stream(session: string, events: any[] = [{ type: 'token', text: 'synthetic answer' }]) {
  return new Response([{ type: 'session', session_id: session }, ...events].map(e => `data: ${JSON.stringify(e)}\n\n`).join(''),
    { headers: { 'Content-Type': 'text/event-stream' } })
}
function binding(id = 'selection-one', body: any = { person_id: 'person-one', device_id: 'device-one' }) {
  return { binding_id: id, session_id: `session-${id}`, scope: { person_id: body.person_id ?? null, device_id: body.device_id ?? null, room: null },
    household_write: body.household_write ?? false, expires_ms: Date.now() + 900000, ttl_ms: 900000, data_epoch: 0 }
}
const apply = 'Apply selection and start new chat'
const revoke = 'Remove selection and start unselected chat'

// Conservative text-only continuity remains usable; a legacy history rejection
// keeps the draft and never silently invents a replacement session.
{
  calls = []; authToken.value = null; localStorage.removeItem('hm.session')
  let conflict = false
  handler = (_path, method, body) => {
    assert.equal(method, 'POST'); assert.ok(!('scope_binding_id' in body))
    return conflict ? { status: 409, body: { error: 'SESSION_SCOPE_UNVERIFIED' } } : stream('headless-one')
  }
  const { root, app } = mount()
  edit(composer(root), 'headless hello'); await nextTick()
  await composer(root).props.onKeydown({ key: 'Enter', preventDefault() {} }); await flush()
  assert.ok(text(root).includes('synthetic answer')); assert.equal(localStorage.getItem('hm.session'), 'headless-one')
  conflict = true
  edit(composer(root), 'keep this draft'); await nextTick()
  await button(root, 'Send').props.onClick(); await flush()
  assert.equal(composer(root).value, 'keep this draft')
  assert.ok(text(root).includes('old history has not been imported'))
  assert.equal(calls.at(-1)?.body.session_id, 'headless-one')
  assert.equal(calls.length, 2)
  await button(root, 'New session').props.onClick(); await flush()
  conflict = false
  await button(root, 'Send').props.onClick(); await flush()
  assert.ok(!('session_id' in calls.at(-1)!.body))
  await choose(root, 'person')
  assert.equal(button(root, apply).props.disabled, true, 'paired authority required to select scope')
  assert.ok(text(root).includes('Pair this browser'))
  app.unmount()
}

// Create -> exact server readback -> new session. Failed create/readback keeps
// both message and selection drafts. Retry readback does not create again.
{
  calls = []; authToken.value = 'hm1_synthetic_browser'; let creationFails = true; let readFails = true
  const selected = binding(); const reading = deferred(); let holdRead = false
  handler = async (path, method, body) => {
    if (path === '/api/v1/chat/scopes') {
      assert.equal(method, 'POST'); assert.deepEqual(body, { person_id: 'person-one', device_id: 'device-one', household_write: false })
      return creationFails ? { status: 400, body: { error: 'synthetic create failure' } } : selected
    }
    if (path.endsWith('/selection-one') && method === 'GET') return holdRead ? reading.promise
      : readFails ? { status: 400, body: { error: 'synthetic readback failure' } } : selected
    if (path === '/api/v0/chat') {
      assert.deepEqual(body, { session_id: selected.session_id, text: 'selected draft', scope_binding_id: selected.binding_id })
      return stream(selected.session_id)
    }
    assert.fail(`unexpected ${method} ${path}`)
  }
  const { root, app } = mount()
  edit(composer(root), 'selected draft')
  await choose(root, 'person-device')
  edit(field(root, 'Person ID'), 'person-one'); edit(field(root, 'Device ID'), 'device-one'); await nextTick()
  assert.equal(button(root, 'Send').props.disabled, true)
  await button(root, apply).props.onClick(); await flush()
  assert.ok(text(root).includes('synthetic create failure')); assert.equal(composer(root).value, 'selected draft')
  creationFails = false
  await button(root, apply).props.onClick(); await flush()
  assert.ok(text(root).includes('synthetic readback failure')); assert.equal(field(root, 'Person ID').value, 'person-one')
  assert.equal(button(root, 'Send').props.disabled, true)
  readFails = false; holdRead = true
  const retrying = button(root, 'Retry selection readback').props.onClick(); await flush()
  assert.equal(button(root, 'Send').props.disabled, true)
  assert.equal(composer(root).value, 'selected draft')
  reading.resolve(selected); await retrying; await flush()
  assert.equal(calls.filter(c => c.path === '/api/v1/chat/scopes').length, 2, 'readback retry cannot create extra binding')
  assert.equal(localStorage.getItem('hm.session'), null, 'selected session cannot leak into unbound reload continuity')
  assert.ok(text(root).includes('Person ID: person-one') && text(root).includes('Device ID: device-one'))
  assert.ok(text(root).includes('Expires'))
  await button(root, 'Send').props.onClick(); await flush()
  assert.ok(calls.every(c => c.headers.Authorization === 'Bearer hm1_synthetic_browser'))
  app.unmount()
}

// Household writes require a deliberate checkbox and verified new session;
// failed revocation blocks sends, while explicit successful removal is fresh.
{
  calls = []; localStorage.removeItem('hm.session'); let next = 0; let current: any; let revokeFails = true
  handler = (path, method, body) => {
    if (path === '/api/v1/chat/scopes') { current = binding(`house-${++next}`, body); return current }
    if (method === 'GET') return current
    if (method === 'DELETE') return revokeFails ? { status: 500, body: { error: 'synthetic revoke failure' } } : { binding_id: current.binding_id, state: 'revoked' }
    return stream('new-headless')
  }
  const { root, app } = mount()
  await choose(root, 'household')
  await button(root, apply).props.onClick(); await flush()
  assert.equal(calls[0].body.household_write, false)
  assert.ok(text(root).includes('No memory saves authorized'))
  const toggle = all(root).find(n => n.tag === 'input' && n.type === 'checkbox')
  toggle.props.onChange({ target: { checked: true } }); await nextTick()
  revokeFails = false
  await button(root, apply).props.onClick(); await flush()
  assert.equal(calls.filter(c => c.path === '/api/v1/chat/scopes').at(-1)?.body.household_write, true)
  assert.ok(text(root).includes('Household saves allowed'))
  revokeFails = true
  edit(composer(root), 'retained across revoke'); await nextTick()
  await button(root, revoke).props.onClick(); await flush()
  assert.ok(text(root).includes('synthetic revoke failure'))
  assert.equal(button(root, 'Send').props.disabled, true); assert.equal(composer(root).value, 'retained across revoke')
  revokeFails = false
  await button(root, revoke).props.onClick(); await flush()
  assert.ok(text(root).includes('No selected person or device'))
  await button(root, 'Send').props.onClick(); await flush()
  assert.ok(!('scope_binding_id' in calls.at(-1)!.body) && !('session_id' in calls.at(-1)!.body))
  app.unmount()
}

// No silent renewal: expiry and streamed authority failures require explicit
// reselection. A message rejected after waiting is preserved for that choice.
{
  calls = []; let clock = 10000; const realNow = Date.now; Date.now = () => clock
  let current: any; let next = 0
  handler = (path, method, body) => {
    if (path === '/api/v1/chat/scopes') { current = { ...binding(`short-${++next}`, body), expires_ms: clock + 1000 }; return current }
    if (method === 'GET') return current
    if (method === 'DELETE') return { status: 409, body: { error: 'SCOPE_BINDING_UNAVAILABLE' } }
    return stream(current.session_id, [{ type: 'error', message: 'scope binding expired or revoked' }])
  }
  const { root, app } = mount()
  await choose(root, 'device'); edit(field(root, 'Device ID'), 'device-one'); await nextTick()
  await button(root, apply).props.onClick(); await flush()
  edit(composer(root), 'preserve on expiry'); await nextTick()
  clock += 2000
  for (const tick of (globalThis as any).__polls.values()) tick(); await flush()
  assert.ok(text(root).includes('expired or is unavailable'))
  assert.equal(button(root, 'Send').props.disabled, true); assert.equal(calls.length, 2)
  await button(root, apply).props.onClick(); await flush()
  assert.equal(calls.filter(c => c.path === '/api/v1/chat/scopes').length, 2)
  await button(root, 'Send').props.onClick(); await flush()
  assert.equal(composer(root).value, 'preserve on expiry')
  assert.equal(button(root, 'Send').props.disabled, true)
  assert.ok(text(root).includes('explicitly') || text(root).includes('Explicitly'))
  locale.value = 'zh'; await nextTick()
  assert.ok(text(root).includes('本次聊天的记忆范围') && text(root).includes('消息草稿已保留'))
  app.unmount(); locale.value = 'en'; Date.now = realNow
}

// Device restrictions are displayed independently from source provenance.
{
  handler = path => path.includes('user-md') ? { entries: [{ id: 'restricted', person_id: '', scope_device: 'device-one', key: 'device-only',
    content: 'synthetic restricted entry', status: 'active', author: 'fixture', source_device: 'source-two', trace_id: '', turn_ts: 0, rationale: '' }] }
    : { content: '', version: 1, revisions: [] }
  const { root, app } = mount(AgentPersonaCard); await flush()
  const row = all(root).find(n => n.tag === 'li' && text(n).includes('device-only'))
  assert.ok(text(row).includes('Device ID: device-one') && text(row).includes('source-two'))
  assert.ok(!text(row).includes('Household'))
  app.unmount()
}

// Notes and pending proposals expose conjunctive visibility separately from
// their origin/source label; management is broader than a turn's prompt.
{
  const restricted = { id: 'note-one', text: 'synthetic note', scope_person: 'person-one', scope_device: 'device-one', kind: 'fact', source: 'voice', pinned: false }
  const proposed = { ...restricted, id: 'candidate-one', text: 'synthetic candidate', status: 'pending', device: 'source-two' }
  handler = path => path.includes('/candidates') ? { items: [proposed] } : { items: [restricted], max: 200 }
  const { root, app } = mount(MemoryCard); await flush()
  const rows = all(root).filter(n => n.tag === 'li')
  assert.equal(rows.length, 2)
  for (const row of rows) {
    assert.ok(text(row).includes('Memory visibility: Person ID: person-one · Device ID: device-one'))
    assert.ok(!text(row).includes('Household'))
  }
  app.unmount()
}

// Both existing physical-session consumers use the opaque current identity
// returned by the server and CAS clear it, never construct dev:* archive IDs.
for (const component of [ConversationCard, FollowUpPanel]) {
  calls = []; let stale = true
  let current: any = { session_id: 'server-owned-opaque-session', messages: [{ role: 'assistant', content: 'Current scoped conversation', ts_ms: 1 }] }
  handler = (path, method, body) => {
    assert.equal(path, '/api/v0/devices/unit-one/conversation')
    if (method === 'GET') return current
    assert.equal(method, 'DELETE'); assert.deepEqual(body, { expected_session_id: 'server-owned-opaque-session' })
    if (stale) return { status: 409, body: { error: 'SESSION_CHANGED' } }
    current = { session_id: current.session_id, messages: [] }
    return { session_id: current.session_id, cleared: 1 }
  }
  const { root, app } = mount(component, { device: { device: 'unit-one', session: null }, mode: {}, refresh() {} }); await flush()
  const clearLabel = 'Clear conversation'
  assert.equal(button(root, clearLabel).props.disabled, false)
  await button(root, clearLabel).props.onClick(); await nextTick()
  await button(portal, clearLabel).props.onClick(); await flush()
  assert.ok(text(root).includes('current device conversation changed'))
  assert.equal(current.messages.length, 1)
  stale = false
  await button(root, clearLabel).props.onClick(); await nextTick()
  await button(portal, clearLabel).props.onClick(); await flush()
  assert.equal(current.messages.length, 0)
  assert.equal(button(root, clearLabel).props.disabled, true)
  current = { session_id: null, messages: [] }
  for (const tick of (globalThis as any).__polls.values()) await tick(); await flush()
  assert.equal(button(root, clearLabel).props.disabled, true, 'no current verified session must not fall back to archive')
  assert.ok(calls.every(c => !c.path.includes('/session/')), 'active consumers must not target archive APIs')
  app.unmount()
}
// The actual conversation SFC/API client keeps current and archived views explicit.
{
  calls = []
  const metadata = { current_context: { generation: 2 }, archived_contexts: [{ session_id: 'archive-one', generation: 1 }] }
  handler = (path, method) => {
    assert.equal(method, 'GET')
    if (path.endsWith('?context_session_id=archive-one')) return { ...metadata, session_id: 'archive-one', view: 'archive', messages: [{ role: 'assistant', content: 'OLD_ARCHIVE_SENTINEL', ts_ms: 1 }] }
    assert.equal(path, '/api/v0/devices/unit-one/conversation')
    return { ...metadata, session_id: 'current-two', view: 'current', messages: [{ role: 'assistant', content: 'CURRENT_CONTEXT_SENTINEL', ts_ms: 2 }] }
  }
  const { root, app } = mount(ConversationCard, { device: { device: 'unit-one' } }); await flush()
  assert.ok(text(root).includes('CURRENT_CONTEXT_SENTINEL'))
  assert.ok(text(root).includes('Archived context 1'))
  const selector = all(root).find(n => n.tag === 'select')
  await selector.props.onChange({ target: { value: 'archive-one' } }); await flush()
  assert.ok(text(root).includes('OLD_ARCHIVE_SENTINEL'))
  assert.equal(button(root, 'Clear conversation').props.disabled, true)
  await selector.props.onChange({ target: { value: '' } }); await flush()
  assert.ok(text(root).includes('CURRENT_CONTEXT_SENTINEL'))
  assert.ok(!text(root).includes('OLD_ARCHIVE_SENTINEL'))
  locale.value = 'zh'; await nextTick()
  assert.ok(text(root).includes('当前上下文'))
  assert.ok(text(root).includes('已归档上下文 1'))
  locale.value = 'en'; app.unmount()
  assert.ok(calls.every(c => c.method === 'GET'), 'reading archive must not clear or renew context')
}
// Bounded page navigation preserves the selected archive and its messages.
{
  calls = []
  handler = (path, method) => {
    assert.equal(method, 'GET')
    const query = new URL(path, 'http://fixture').searchParams
    const selected = query.get('context_session_id')
    return { session_id: selected ?? 'current-100', current_context: { generation: 100 }, selected_context: { generation: selected ? 99 : 100 }, view: selected ? 'archive' : 'current',
      archived_contexts: query.has('archive_before') ? [{ session_id: 'archive-79', generation: 79 }] : [{ session_id: 'archive-99', generation: 99 }],
      next_archive_before: query.has('archive_before') ? null : 99,
      next_message_before: query.has('message_before') ? null : 500,
      messages: [{ role: 'assistant', content: query.has('message_before') ? 'OLDER_MESSAGE_PAGE' : selected ? 'SELECTED_ARCHIVE_99' : 'CURRENT_100', ts_ms: 1 }] }
  }
  const { root, app } = mount(ConversationCard, { device: { device: 'unit-one' } }); await flush()
  const selector = all(root).find(n => n.tag === 'select')
  await selector.props.onChange({ target: { value: 'archive-99' } }); await flush()
  await button(root, 'Earlier contexts').props.onClick(); await flush()
  assert.ok(text(root).includes('SELECTED_ARCHIVE_99'))
  assert.equal(all(root).find(n => n.tag === 'select').props.value, 'archive-99')
  assert.ok(text(root).includes('Archived context 99'), 'selected archive survives page navigation')
  assert.equal(button(root, 'Clear conversation').props.disabled, true)
  await button(root, 'Earlier messages').props.onClick(); await flush()
  assert.ok(text(root).includes('OLDER_MESSAGE_PAGE'))
  await button(root, 'Latest messages').props.onClick(); await flush()
  assert.ok(text(root).includes('SELECTED_ARCHIVE_99'))
  await button(root, 'Latest contexts').props.onClick(); await flush()
  assert.equal(all(root).find(n => n.tag === 'select').props.value, 'archive-99')
  assert.ok(calls.some(c => c.path.includes('archive_before=99')))
  assert.ok(calls.some(c => c.path.includes('message_before=500')))
  locale.value = 'zh'; await nextTick()
  assert.ok(text(root).includes('更早上下文'))
  assert.ok(text(root).includes('更早消息'))
  locale.value = 'en'; app.unmount()
}
console.log('Chat scope UI: selection/readback/grants/revoke/expiry/SSE rejection/headless history/scoped visibility/current physical conversation and bilingual checks passed')
