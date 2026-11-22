import assert from 'node:assert/strict'
import { createRenderer, nextTick } from 'vue'
import { createMemoryHistory, createRouter } from 'vue-router'
import ConfirmationCard from '../src/components/confirmations/ConfirmationCard.vue'
import ConfirmationNotice from '../src/components/confirmations/ConfirmationNotice.vue'
import ConfirmationPage from '../src/views/ConfirmationPage.vue'
import ChatCard from '../src/components/diagnostics/ChatCard.vue'
import { authToken } from '../src/auth'
import { locale } from '../src/i18n'
import { getConfirmation, transitionConfirmation, type ConfirmationAction } from '../src/api'
import { confirmationActions, confirmationBusy, confirmationErrors, receiveConfirmation,
  refreshConfirmation, refreshConfirmations } from '../src/stores/confirmations'

function node(tag = '', text = ''): any {
  return { tag, text, children: [], parent: null, props: {}, tagName: tag.toUpperCase(),
    addEventListener() {}, removeEventListener() {}, setAttribute() {}, removeAttribute() {},
    getRootNode() { return document }, querySelector() { return null }, querySelectorAll() { return [] }, focus() {},
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
const all = (root: any): any[] => [root, ...root.children.flatMap(all)]
const text = (root: any): string => root.tag === '#comment' ? '' : root.text + root.children.map(text).join('')
function button(root: any, label: string): any {
  const result = all(root).find(n => n.tag === 'button' && text(n).trim() === label)
  assert.ok(result, `button ${label} must render`); return result
}
function hasButton(root: any, label: string): boolean { return all(root).some(n => n.tag === 'button' && text(n).trim() === label) }
async function flush() { for (let i = 0; i < 25; i++) { await Promise.resolve(); await nextTick() } }
async function mount(component: any = ConfirmationCard, props: any = { id: 'action-one' }, path = '/') {
  const router = createRouter({ history: createMemoryHistory(), routes: [
    { path: '/', component: { render: () => null } },
    { path: '/confirmations', component: ConfirmationPage },
  ] })
  await router.push(path); await router.isReady()
  const root = node('root'); const app = renderer.createApp(component, props); app.use(router); app.mount(root)
  await flush(); return { root, app }
}
function deferred() { let resolve!: (v: any) => void; const promise = new Promise(r => { resolve = r }); return { promise, resolve } }
function action(overrides: Partial<ConfirmationAction> = {}): ConfirmationAction {
  return { id: 'action-one', revision: 1, state: 'pending', task_id: 'task-one', call_id: 'call-one', session_id: 'session-one',
    created_ms: Date.now(), expires_ms: Date.now() + 300000, ttl_ms: 300000,
    preview: { tool: 'openapi.call', operation: 'remove-synthetic-item', service: 'Synthetic service', method: 'DELETE',
      target: 'http://127.0.0.1/synthetic/item-one', parameters: { item: 'item-one', token: '[REDACTED]' }, scope: { device_id: 'unit-one' } },
    requester: { kind: 'physical', device_id: 'unit-one' }, approver_policy: 'paired_management', approved_ms: null,
    consumed_ms: null, outcome: null, ...overrides }
}
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
function reset() { authToken.value = null; authToken.value = 'hm1_synthetic_confirmation'; calls = []; locale.value = 'en' }
const approve = 'Approve only', execute = 'Execute approved action', refresh = 'Refresh action state'

// The actual client sends a real bearer and exactly one revision CAS body.
{
  reset(); const current = action({ id: 'id/with slash' })
  handler = (path, method, body) => {
    assert.equal(path, '/api/v1/confirmations/id%2Fwith%20slash/approve'); assert.equal(method, 'POST')
    assert.deepEqual(body, { expected_state: 'pending', expected_revision: 1 }); return { ...current, revision: 2, state: 'approved' }
  }
  await transitionConfirmation(current, 'approve')
  assert.equal(calls.length, 1); assert.equal(calls[0].headers.Authorization, 'Bearer hm1_synthetic_confirmation')
}

// Mounted production SFC: approval and execution are separate gestures. A
// double click cannot dispatch a second mutation while the first is in flight.
{
  reset(); let current = action(); const admitted = deferred(); let executions = 0
  handler = (path, method, body) => {
    if (method === 'GET') return current
    assert.deepEqual(Object.keys(body).sort(), ['expected_revision', 'expected_state'])
    if (path.endsWith('/approve')) { assert.equal(executions, 0); return current = { ...current, state: 'approved', revision: 2 } }
    if (path.endsWith('/execute')) { executions++; assert.deepEqual(body, { expected_state: 'approved', expected_revision: 2 }); return admitted.promise }
    assert.fail(`unexpected ${path}`)
  }
  const { root, app } = await mount()
  assert.ok(text(root).includes('DELETE http://127.0.0.1/synthetic/item-one'))
  assert.ok(text(root).includes('[REDACTED]') && /Speaker\s*·\s*unit-one/.test(text(root)))
  await button(root, approve).props.onClick(); await flush()
  assert.equal(executions, 0); assert.ok(text(root).includes('Approved — not executed'))
  assert.ok(!text(root).includes('target state confirmed'))
  const first = button(root, execute).props.onClick(); const second = button(root, execute).props.onClick(); await flush()
  assert.equal(executions, 1); assert.equal(button(root, execute).props.disabled, true)
  current = { ...current, state: 'consumed', revision: 3, consumed_ms: Date.now(), outcome: { status: 'running', evidence: 'accepted', result: null } }
  admitted.resolve(current); await Promise.all([first, second]); await flush()
  assert.ok(text(root).includes('Execution admitted — awaiting outcome')); assert.ok(!hasButton(root, execute))
  current = { ...current, revision: 4, outcome: { status: 'completed', evidence: 'sent', result: null } }
  await button(root, refresh).props.onClick(); await flush()
  assert.ok(text(root).includes('target state is unconfirmed'))
  current = { ...current, revision: 5, outcome: { status: 'unknown', evidence: 'sent', result: null, error: { code: 'LOST_RESPONSE', detail: 'Synthetic response lost', retryable: false } } }
  await button(root, refresh).props.onClick(); await flush()
  assert.ok(text(root).includes('Outcome unknown') && text(root).includes('will not retry'))
  for (const tick of (globalThis as any).__polls.values()) tick(); await flush()
  assert.equal(executions, 1); assert.ok(!all(root).some(n => n.tag === 'button' && /retry/i.test(text(n))))
  locale.value = 'zh'; await nextTick()
  assert.ok(text(root).includes('结果未知') && text(root).includes('控制台不会重试'))
  app.unmount()
}

// A lost POST response preserves the original preview. Failed GET stays
// stale/disabled; only a successful server readback reconciles the action.
{
  reset(); let current = action({ state: 'approved', revision: 2 }); let readsFail = false; let sends = 0
  handler = (_path, method) => {
    if (method === 'POST') { sends++; throw new Error('synthetic lost response') }
    return readsFail ? { status: 500, body: { error: 'synthetic server failure' } } : current
  }
  const { root, app } = await mount()
  await button(root, execute).props.onClick(); await flush()
  assert.equal(sends, 1); assert.ok(text(root).includes('last preview is kept'))
  assert.ok(text(root).includes('item-one')); assert.equal(button(root, execute).props.disabled, true)
  readsFail = true; await button(root, refresh).props.onClick(); await flush()
  assert.ok(text(root).includes('synthetic server failure')); assert.equal(button(root, execute).props.disabled, true)
  current = { ...current, revision: 3, state: 'stale' }; readsFail = false
  await button(root, refresh).props.onClick(); await flush()
  assert.ok(text(root).includes('Invalidated')); assert.ok(!hasButton(root, execute)); assert.equal(sends, 1)
  app.unmount()
}

// Reject and cancel use the server transition and display no execution fact.
for (const [label, transition, terminal] of [['Reject', 'reject', 'rejected'], ['Cancel request', 'cancel', 'cancelled']] as const) {
  reset(); let current = action()
  handler = (path, method, body) => method === 'GET' ? current : (() => {
    assert.ok(path.endsWith(`/${transition}`)); assert.deepEqual(body, { expected_state: 'pending', expected_revision: 1 })
    return current = { ...current, revision: 2, state: terminal }
  })()
  const { root, app } = await mount(); await button(root, label).props.onClick(); await flush()
  assert.ok(text(root).includes('not executed')); assert.ok(!hasButton(root, execute)); app.unmount()
}

// Local expiry disables execution without inventing a server transition;
// restart/cancel and conflict are read back from the actual client path.
{
  reset(); const realNow = Date.now; let clock = 10000; Date.now = () => clock
  let current = action({ state: 'approved', expires_ms: clock + 1000 })
  handler = () => current
  const { root, app } = await mount(); clock += 2000
  for (const tick of (globalThis as any).__polls.values()) tick(); await flush()
  assert.equal(button(root, execute).props.disabled, true); assert.ok(text(root).includes('validity period has ended'))
  assert.ok(calls.every(c => c.method === 'GET'))
  current = { ...current, state: 'cancelled', revision: 2 }; await button(root, refresh).props.onClick(); await flush()
  assert.ok(text(root).includes('Cancelled — not executed')); app.unmount(); Date.now = realNow
}
{
  reset(); let current = action(); let conflict = true
  handler = (_path, method) => method === 'POST' && conflict ? { status: 409, body: { error: 'CONFIRMATION_STALE', action: { ...current, state: 'stale', revision: 2 } } } : current
  const { root, app } = await mount(); await button(root, approve).props.onClick(); await flush()
  assert.ok(text(root).includes('CONFIRMATION_STALE')); assert.equal(button(root, approve).props.disabled, true)
  current = { ...current, state: 'stale', revision: 2 }; conflict = false
  await button(root, refresh).props.onClick(); await flush(); assert.ok(text(root).includes('Invalidated')); app.unmount()
}

// Late pre-transition GET cannot roll back newer server revisions.
{
  reset(); const older = action(); const delayed = deferred(); handler = () => delayed.promise
  receiveConfirmation(older); const reading = refreshConfirmation(older.id)
  receiveConfirmation({ ...older, state: 'consumed', revision: 3 })
  delayed.resolve(older); await reading
  assert.equal(confirmationActions.get(older.id)?.state, 'consumed')
}

// A delayed consumed/running read cannot replace a newer completed result.
// Likewise, an older display payload cannot restore a server-scrubbed preview.
{
  reset(); const initial = action({ state: 'consumed', revision: 3,
    outcome: { status: 'running', evidence: 'accepted', result: null } })
  let response: any = initial; handler = () => response
  const { root, app } = await mount()
  const delayed = deferred(); response = delayed.promise
  const reading = refreshConfirmation(initial.id)
  const completed = { ...initial, revision: 4,
    outcome: { status: 'completed' as const, evidence: 'sent' as const, result: null } }
  receiveConfirmation(completed); await nextTick()
  assert.ok(text(root).includes('target state is unconfirmed'))
  delayed.resolve(initial); await reading; await flush()
  assert.equal(confirmationActions.get(initial.id)?.revision, 4)
  assert.equal(confirmationActions.get(initial.id)?.outcome?.status, 'completed')
  assert.ok(text(root).includes('Execution outcome recorded'))
  assert.ok(!text(root).includes('Execution admitted — awaiting outcome'))
  const oldPreview = deferred(); response = oldPreview.promise
  const scrubRead = refreshConfirmation(initial.id)
  const scrubbed = { ...completed, revision: 5, preview: { ...completed.preview,
    target: completed.preview.service, parameters: {}, scope: {} } }
  receiveConfirmation(scrubbed); await nextTick()
  assert.ok(!text(root).includes('item-one'))
  oldPreview.resolve(completed); await scrubRead; await flush()
  assert.equal(confirmationActions.get(initial.id)?.revision, 5)
  assert.deepEqual(confirmationActions.get(initial.id)?.preview.parameters, {})
  assert.ok(!text(root).includes('item-one'), 'old GET must not restore scrubbed target/parameters')
  app.unmount()
}

// Revoked pairing clears cached previews and rejects late old-principal GET.
{
  reset(); const delayed = deferred(); handler = () => delayed.promise
  receiveConfirmation(action()); const reading = refreshConfirmation('action-one'); authToken.value = null
  delayed.resolve(action()); await reading
  assert.equal(confirmationActions.size, 0); assert.equal(confirmationErrors.size, 0); assert.equal(confirmationBusy.size, 0)
  handler = () => { assert.fail('unpaired UI must not send a request') }
  const { root, app } = await mount(); assert.equal(button(root, refresh).props.disabled, true); app.unmount()
}
{
  reset(); handler = () => ({ status: 401, body: { error: 'PAIRING_REQUIRED' } })
  await assert.rejects(getConfirmation('missing')); assert.equal(authToken.value, null)
}

// The ordinary shell notice finds a physical request via real list endpoints;
// the page opens a directly linked action, including after a browser refresh.
{
  reset(); const current = action()
  handler = path => path.includes('?') ? { actions: path.includes('pending') ? [current] : [], next_cursor: null } : current
  const { root, app } = await mount(ConfirmationNotice, {})
  assert.ok(text(root).includes('1 action(s) need your decision'))
  assert.equal(all(root).find(n => n.tag === 'a')?.props.href, '/confirmations')
  assert.ok(calls.every(c => c.method === 'GET')); app.unmount()
  const page = await mount(ConfirmationPage, {}, '/confirmations?action=action-one')
  assert.ok(/Speaker\s*·\s*unit-one/.test(text(page.root))); page.app.unmount()
}

// Actual chat consumes the production SSE shape and keeps cancellation after
// the stream has ended. Neither a pending fact nor approval looks completed.
{
  reset(); localStorage.removeItem('hm.session'); const current = action(); let cancelled = false
  handler = (path, method, body) => {
    if (path === '/api/v0/chat') return new Response([
      { type: 'session', session_id: current.session_id }, { type: 'task', task_id: current.task_id },
      { type: 'tool_start', tool: 'openapi.call' },
      { type: 'tool_result', tool: 'openapi.call', status: 'awaiting_confirmation', summary: 'Awaiting your decision' },
      { type: 'confirmation_required', action: current },
      { type: 'turn_end', status: 'confirmation_required', trace_id: 'trace-one' },
    ].map(e => `data: ${JSON.stringify(e)}\n\n`).join(''), { headers: { 'Content-Type': 'text/event-stream' } })
    if (path === '/api/v0/session/cancel') { assert.equal(method, 'POST'); assert.deepEqual(body, { task_id: current.task_id }); cancelled = true; return { state: 'cancelled' } }
    if (path.endsWith('/action-one')) return cancelled ? { ...current, state: 'cancelled', revision: 2 } : current
    assert.fail(`unexpected ${path}`)
  }
  const { root, app } = await mount(ChatCard, {})
  const composer = all(root).find(n => n.props.id === 'chat-input')
  composer.props['onUpdate:modelValue']('remove synthetic item'); await nextTick()
  await button(root, 'Send').props.onClick(); await flush()
  assert.ok(text(root).includes('separate Execute action'))
  assert.ok(all(root).some(n => n.tag === 'a' && n.props.href === '/confirmations?action=action-one'))
  composer.props['onUpdate:modelValue']('keep my next message'); await nextTick()
  await button(root, 'Cancel pending task').props.onClick(); await flush()
  assert.equal(cancelled, true); assert.ok(text(root).includes('Cancelled — not executed'))
  assert.equal(composer.value, 'keep my next message'); app.unmount()
}
assert.equal((globalThis as any).__polls.size, 0, 'all SFC polling and clocks must be disposed')
console.log('PASS confirmation client, mounted cards, chat, pairing, lifecycle and bilingual behavior')
