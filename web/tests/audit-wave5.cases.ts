import assert from 'node:assert/strict'
import { createRenderer, defineComponent, h, nextTick, ref } from 'vue'
import { createMemoryHistory, createRouter } from 'vue-router'
import App from '../src/App.vue'
import AuthGate from '../src/components/AuthGate.vue'
import NumberField from '../src/components/NumberField.vue'
import TextInput from '../src/components/TextInput.vue'
import InterruptStylePanel from '../src/components/interaction/InterruptStylePanel.vue'
import DuplexCard from '../src/components/interaction/DuplexCard.vue'
import AuthCard from '../src/components/settings/AuthCard.vue'
import MaintenanceCard from '../src/components/settings/MaintenanceCard.vue'
import ChatCard from '../src/components/diagnostics/ChatCard.vue'
import LedCard from '../src/components/device/LedCard.vue'
import ToneCard from '../src/components/device/ToneCard.vue'
import VolumeCard from '../src/components/device/VolumeCard.vue'
import StoryJokeCard from '../src/components/tools/StoryJokeCard.vue'
import OpenSocketsCard from '../src/components/tools/OpenSocketsCard.vue'
import ProviderCard from '../src/components/service/ProviderCard.vue'
import WizardServiceCard from '../src/components/wizard/WizardServiceCard.vue'
import { ApiError, postChat } from '../src/api'
import { authToken, clearAuthToken, markUnauthorized, pairingRequired, setAuthToken } from '../src/auth'
import { setAdvancedMode } from '../src/stores/advanced'
import { locale } from '../src/i18n'

function node(tag = '', text = ''): any {
  const n: any = {
    tag,
    text,
    children: [],
    parent: null,
    props: {},
    listeners: {},
    tagName: tag.toUpperCase(),
    selected: false,
    get options() {
      return this.children.filter((c: any) => c.tag === 'option')
    },
    addEventListener(name: string, fn: unknown) {
      this.listeners[name] = fn
    },
    removeEventListener(name: string) {
      delete this.listeners[name]
    },
    setAttribute(name: string, value: unknown) {
      this.props[name] = value
    },
    removeAttribute(name: string) {
      delete this.props[name]
    },
    getRootNode() {
      return document
    },
    querySelectorAll() {
      return []
    },
    querySelector() {
      return null
    },
    focus() {},
    getClientRects() {
      return [1]
    },
  }
  if (tag === 'select') {
    n.selectedIndex = -1
    Object.defineProperty(n, 'value', {
      get() {
        const picked =
          n.selectedIndex >= 0 ? n.options[n.selectedIndex] : n.options.find((o: any) => o.selected)
        return picked ? (picked._value ?? picked.value ?? picked.props.value) : n.props.value
      },
      set(v: unknown) {
        n.props.value = v
        const idx = n.options.findIndex((o: any) => (o._value ?? o.value ?? o.props.value) === v)
        n.selectedIndex = idx
        for (let i = 0; i < n.options.length; i++) {
          n.options[i].selected = i === idx
        }
      },
    })
  }
  return n
}

const teleportTarget = node('body')
const renderer = createRenderer<any, any>({
  querySelector: () => teleportTarget,
  createElement: (tag) => node(tag),
  createText: (text) => node('#text', text),
  createComment: (text) => node('#comment', text),
  insert(el, parent, anchor) {
    el.parent = parent
    const at = parent.children.indexOf(anchor)
    if (at < 0) parent.children.push(el)
    else parent.children.splice(at, 0, el)
  },
  remove(el) {
    if (el.parent) el.parent.children.splice(el.parent.children.indexOf(el), 1)
  },
  setText(el, text) {
    el.text = text
  },
  setElementText(el, text) {
    el.text = text
    el.children = []
  },
  parentNode: (el) => el.parent,
  nextSibling: (el) => el.parent?.children[el.parent.children.indexOf(el) + 1] ?? null,
  insertStaticContent: (c) => {
    const n = node('#static', c)
    return [n, n]
  },
  patchProp(el, key, _old, value) {
    el.props[key] = value
    if (key === 'type' || key === 'value' || key === 'checked') el[key] = value
  },
})

function all(root: any): any[] {
  return [root, ...root.children.flatMap(all)]
}
function text(root: any): string {
  return root.tag === '#comment' ? '' : root.text + root.children.map(text).join('')
}
function button(root: any, label: string): any {
  const found = all(root).find((n) => n.tag === 'button' && text(n).trim().includes(label))
  assert.ok(
    found,
    `button "${label}" must render; got: ${all(root)
      .filter((n) => n.tag === 'button')
      .map((b) => text(b).trim())
      .join(' | ')}`,
  )
  return found
}
function edit(el: any, value: unknown) {
  el.props['onUpdate:modelValue']?.(value)
  el.props.onInput?.({ target: { value } })
  el.props.onChange?.({ target: { value, checked: value } })
}
async function flush() {
  for (let i = 0; i < 16; i++) {
    await Promise.resolve()
    await nextTick()
  }
}

let calls: { path: string; method: string; body: any; signal?: AbortSignal }[] = []
let handler: (path: string, method: string, body: any, init: any) => any = () => ({})
Object.assign(globalThis, {
  fetch: async (path: string, init: any = {}) => {
    const method = init.method ?? 'GET'
    const body = init.body ? JSON.parse(init.body) : null
    calls.push({ path, method, body, signal: init.signal })
    const result = await handler(path, method, body, init)
    if (result instanceof Response) return result
    const envelope =
      result && typeof result === 'object' && typeof result.status === 'number' && Object.hasOwn(result, 'body')
    return new Response(JSON.stringify(envelope ? result.body : result), {
      status: envelope ? result.status : 200,
      headers: { 'Content-Type': 'application/json' },
    })
  },
})

// ── P0-1 & P1-8: First-boot setup poll paused, one-time token preserved, setup error shown ──
{
  locale.value = 'en'
  clearAuthToken()
  pairingRequired.value = false
  calls = []
  let configured = false
  let failProvisionOnce = true

  handler = (path, method) => {
    if (path === '/api/v0/auth' && method === 'GET') {
      return { configured, corrupt: false, tokens: [], loopback_trusted: false }
    }
    if (path === '/api/v0/auth/provision' && method === 'POST') {
      if (failProvisionOnce) {
        failProvisionOnce = false
        return { status: 500, body: { error: 'disk read-only during provision' } }
      }
      configured = true
      return { token: 'hm1_first_boot_one_time_token', created_ms: 1000, note: 'once' }
    }
    if (path.startsWith('/api/v0/status')) {
      return configured && authToken.value === 'hm1_first_boot_one_time_token'
        ? { product: 'home-mate', assistant_name: 'Home Mate', version: '1.0.0', services: {}, gateway: { connected: true, generation: 1, bind_addr: '127.0.0.1:8932', devices: [], pending_plays: 0 } }
        : { status: 401, body: { error: 'unauthorized' } }
    }
    if (path.startsWith('/api/v0/mode')) return { default_duplex: 'half', wake_words: [], devices: [] }
    if (path === '/api/v0/devices') return { gateway: { connected: true, generation: 1, bind_addr: '127.0.0.1:8932', devices: [], pending_plays: 0 }, recent: [], voice_log: [] }
    if (path === '/api/v0/device-meta') return { meta: {} }
    if (path.startsWith('/api/v1/confirmations')) return { actions: [] }
    return {}
  }

  const router = createRouter({
    history: createMemoryHistory('/'),
    routes: [{ path: '/:p(.*)*', component: defineComponent({ render: () => h('div', 'shell-ok') }) }],
  })
  const root = node('root')
  const app = renderer.createApp(App)
  app.use(router)
  await router.isReady()
  app.mount(root)
  await flush()

  // Background status poll must NOT fire while in setup!
  for (const poll of (globalThis as any).__polls.values()) await poll()
  await flush()
  assert.equal(
    calls.some((c) => c.path.startsWith('/api/v0/status')),
    false,
    'P0-1: status poll must stay paused while authView !== ready',
  )

  // P1-8: non-409 provision error renders in setup without flipping away
  await button(root, 'Create household token').props.onClick()
  await flush()
  assert.ok(text(root).includes('disk read-only during provision'), 'P1-8: setup error is rendered')

  // Second attempt succeeds and displays one-time token
  await button(root, 'Create household token').props.onClick()
  await flush()
  assert.ok(text(root).includes('hm1_first_boot_one_time_token'), 'issued one-time token is visible')

  // Background polls + stray unauthorized signal while in setup must NOT destroy issued token
  for (const poll of (globalThis as any).__polls.values()) await poll()
  markUnauthorized()
  await flush()
  assert.ok(
    text(root).includes('hm1_first_boot_one_time_token'),
    'P0-1: issued token must survive background polls and unauthorized signals during setup',
  )

  // Acknowledge and enter shell -> status loads immediately
  const ack = all(root).find((n) => n.tag === 'input' && n.props.type === 'checkbox')
  assert.ok(ack, 'ack checkbox rendered')
  ack.props['onUpdate:modelValue']?.(true)
  ack.props.onChange?.({ target: { checked: true } })
  await nextTick()
  await button(root, 'Open the console').props.onClick()
  await flush()
  assert.equal(authToken.value, 'hm1_first_boot_one_time_token')
  assert.ok(text(root).includes('shell-ok'), 'entered shell cleanly')
  assert.ok(
    calls.some((c) => c.path.startsWith('/api/v0/status')),
    'status loads immediately on entering ready shell',
  )
  app.unmount()
}

// ── P0-2 & P1-7: Factory Reset clears token & transitions to setup; AuthGate.pair refuses unpaired server; ConfirmDialog closes on error ──
{
  locale.value = 'en'
  setAuthToken('hm1_active_token')
  setAdvancedMode(true)
  calls = []
  let configured = true
  let failResetOnce = true

  handler = (path, method) => {
    if (path === '/api/v0/auth' && method === 'GET') {
      return { configured, corrupt: false, tokens: [], loopback_trusted: false }
    }
    if (path === '/api/v0/system/factory-reset' && method === 'POST') {
      if (failResetOnce) {
        failResetOnce = false
        return { status: 500, body: { error: 'reset storage locked' } }
      }
      configured = false
      return { state: 'reset', secrets_preserved: true }
    }
    if (path.startsWith('/api/v0/status')) {
      return { product: 'home-mate', assistant_name: 'Home Mate', version: '1.0.0', services: {}, gateway: { connected: true, generation: 1, bind_addr: '127.0.0.1:8932', devices: [], pending_plays: 0 } }
    }
    if (path.startsWith('/api/v0/mode')) return { default_duplex: 'half', wake_words: [], devices: [] }
    if (path === '/api/v0/devices') return { gateway: { connected: true, generation: 1, bind_addr: '127.0.0.1:8932', devices: [], pending_plays: 0 }, recent: [], voice_log: [] }
    if (path === '/api/v0/device-meta') return { meta: {} }
    if (path.startsWith('/api/v1/confirmations')) return { actions: [] }
    return {}
  }

  const router = createRouter({
    history: createMemoryHistory('/'),
    routes: [{ path: '/:p(.*)*', component: MaintenanceCard }],
  })
  const root = node('root')
  const app = renderer.createApp(App)
  app.use(router)
  await router.isReady()
  app.mount(root)
  await flush()

  // P1-7: failed factory reset closes ConfirmDialog and exposes error message
  await button(root, 'Factory reset').props.onClick()
  await flush()
  const confirmBtn1 = all(teleportTarget).filter((n) => n.tag === 'button' && text(n).trim() === 'Reset configuration').at(-1)
  assert.ok(confirmBtn1)
  await confirmBtn1.props.onClick()
  await flush()
  assert.equal(
    all(teleportTarget).some((n) => n.tag === 'button' && text(n).trim() === 'Reset configuration'),
    false,
    'P1-7: ConfirmDialog closes in finally even when factory reset fails',
  )
  assert.ok(text(root).includes('reset storage locked'), 'P1-7: error message revealed after dialog closes')

  // P0-2: successful factory reset clears token, triggers bootAuth(), and lands cleanly on setup
  await button(root, 'Factory reset').props.onClick()
  await flush()
  const confirmBtn2 = all(teleportTarget).filter((n) => n.tag === 'button' && text(n).trim() === 'Reset configuration').at(-1)
  assert.ok(confirmBtn2)
  await confirmBtn2.props.onClick()
  await flush()
  assert.equal(authToken.value, null, 'P0-2: factory reset clears browser authToken')
  assert.ok(
    text(root).includes('Create household token'),
    'P0-2: App.vue transitions cleanly to setup gate after factory reset',
  )
  app.unmount()
  setAdvancedMode(false)

  // P0-2 part 2: AuthGate.pair() checks getAuthStatus() and emits retry on unpaired server
  calls = []
  let retried = false
  let readied = false
  handler = (path) => {
    if (path === '/api/v0/auth') return { configured: false, corrupt: false, tokens: [], loopback_trusted: false }
    return { status: 200, body: {} }
  }
  const gateRoot = node('root')
  const gateApp = renderer.createApp(AuthGate, {
    state: 'pair',
    onRetry: () => { retried = true },
    onReady: () => { readied = true },
  })
  gateApp.mount(gateRoot)
  await flush()
  const pw = all(gateRoot).find((n) => n.tag === 'input' && n.props.type === 'password')
  edit(pw, 'hm1_bogus_token')
  await nextTick()
  await button(gateRoot, 'Pair').props.onClick()
  await flush()
  assert.equal(retried, true, 'P0-2: AuthGate.pair emits retry when server is unpaired')
  assert.equal(readied, false, 'P0-2: AuthGate.pair must not accept arbitrary token against unpaired server')
  assert.equal(authToken.value, null)
  gateApp.unmount()
}

// ── P1-1: NumberField decimal input & blur preserves 0.25 and rounds cleanly ──
{
  const val = ref(0.25)
  const root = node('root')
  const app = renderer.createApp({
    setup: () => () =>
      h(NumberField, {
        modelValue: val.value,
        'onUpdate:modelValue': (v: number) => { val.value = v },
        label: 'Threshold',
        min: 0.05,
        max: 0.9,
        step: 0.01,
      }),
  })
  app.mount(root)
  await flush()

  const inp = all(root).find((n) => n.tag === 'input')
  assert.equal(inp.props.inputmode, 'decimal')
  inp.props.onFocus?.()
  inp.props.onInput?.({ target: { value: '0.' } })
  await nextTick()
  inp.props.onInput?.({ target: { value: '0.25' } })
  await nextTick()
  inp.props.onBlur?.()
  await nextTick()
  assert.equal(val.value, 0.25, 'P1-1: decimal 0.25 must not truncate to 0')
  assert.equal(inp.props.value, '0.25')

  inp.props.onFocus?.()
  inp.props.onInput?.({ target: { value: '0.186' } })
  inp.props.onBlur?.()
  await nextTick()
  assert.equal(val.value, 0.19, 'P1-1: blur rounds cleanly to step precision (0.01)')
  assert.equal(inp.props.value, '0.19')
  app.unmount()

  // Integer default unchanged
  const intVal = ref(10)
  const intRoot = node('root')
  const intApp = renderer.createApp({
    setup: () => () =>
      h(NumberField, {
        modelValue: intVal.value,
        'onUpdate:modelValue': (v: number) => { intVal.value = v },
        label: 'Count',
        min: 0,
        max: 100,
      }),
  })
  intApp.mount(intRoot)
  await flush()
  const intInp = all(intRoot).find((n) => n.tag === 'input')
  assert.equal(intInp.props.inputmode, 'numeric')
  intInp.props.onFocus?.()
  intInp.props.onInput?.({ target: { value: '12.5' } })
  intInp.props.onBlur?.()
  await nextTick()
  assert.equal(intVal.value, 10, 'integer NumberField still rejects decimal input')
  intApp.unmount()
}

// ── P1-2: InterruptStylePanel preserves spoken.detail_words and spoken.long_form_words ──
{
  locale.value = 'en'
  calls = []
  const modeSnap: any = {
    default_duplex: 'half',
    wake_words: ['小智'],
    keyword_interrupt: true,
    stop_intents: { enabled: true, stop_words: ['停'], continue_words: ['继续'] },
    spoken: {
      max_sentences: 2,
      max_chars: 120,
      detail_max_sentences: 6,
      detail_max_chars: 300,
      detail_words: ['详细', '展开说说'],
      long_form_words: ['讲个故事', '读一篇'],
      backstop_max_sentences: 400,
      backstop_max_chars: 2000,
      continue_hint: '说继续听后面',
      resume_lead: '接着说',
    },
    truncation_notice: '已截断',
    filler: { delay_ms: 300, tool_delay_ms: 600, mode: 'radio', thinking: ['嗯'], search: [], device: [], story: [] },
    devices: [{ device: 'dot2-a', duplex: { capability: 'half', requested: 'half', negotiated: 'half', effective: 'half' }, participation: 'idle', proactive_speech: false, activity: 'idle', outputs: [] }],
  }
  handler = (path, method) => {
    if (path === '/api/v0/reflex' && method === 'GET') {
      return { words: [{ text: '暂停', tokens: 'z àn t íng', intent: 'pause', enabled: true }], compiled: '', hash: 'h1', warnings: [] }
    }
    if (path === '/api/v0/mode' && method === 'PUT') return modeSnap
    return {}
  }

  const root = node('root')
  const app = renderer.createApp(InterruptStylePanel, {
    mode: modeSnap,
    device: modeSnap.devices[0],
    refresh: async () => {},
  })
  app.mount(root)
  await flush()

  await button(root, 'Save reply style & cues').props.onClick()
  await flush()
  const put = calls.find((c) => c.path === '/api/v0/mode' && c.method === 'PUT')
  assert.ok(put, 'PUT /api/v0/mode was called')
  assert.deepEqual(put.body.spoken.detail_words, ['详细', '展开说说'], 'P1-2: detail_words preserved')
  assert.deepEqual(put.body.spoken.long_form_words, ['讲个故事', '读一篇'], 'P1-2: long_form_words preserved')
  app.unmount()
}

// ── P1-4: AuthCard.forget() triggers markUnauthorized() directly ──
{
  locale.value = 'en'
  setAuthToken('hm1_browser_token')
  assert.equal(pairingRequired.value, false)
  handler = (path) => {
    if (path === '/api/v0/auth') return { configured: true, corrupt: false, tokens: [], loopback_trusted: false }
    return {}
  }
  const root = node('root')
  const app = renderer.createApp(AuthCard)
  app.mount(root)
  await flush()

  await button(root, 'Forget on this browser').props.onClick()
  await flush()
  assert.equal(authToken.value, null, 'P1-4: forget clears authToken')
  assert.equal(pairingRequired.value, true, 'P1-4: forget directly sets pairingRequired=true')
  pairingRequired.value = false
  app.unmount()
}

// ── P1-5: ChatCard.stop() aborts stream cleanly + postChat trailing EOF flush & timeout ──
{
  locale.value = 'en'
  calls = []
  let abortedBeforeTask = false
  handler = (path, _method, _body, init) => {
    if (path === '/api/v0/chat') {
      const stream = new ReadableStream({
        start(controller) {
          init.signal?.addEventListener('abort', () => {
            abortedBeforeTask = true
            controller.error(new DOMException('Aborted', 'AbortError'))
          })
        },
      })
      return new Response(stream, { headers: { 'Content-Type': 'text/event-stream' } })
    }
    return {}
  }

  const root = node('root')
  const app = renderer.createApp(ChatCard)
  app.mount(root)
  await flush()
  const composer = all(root).find((n) => n.props.id === 'chat-input')
  edit(composer, 'hello before task')
  await nextTick()
  const sending = button(root, 'Send').props.onClick()
  await flush()
  // Stop BEFORE any `task` event arrives!
  await button(root, 'Stop').props.onClick()
  await sending
  await flush()
  assert.equal(abortedBeforeTask, true, 'P1-5: ChatCard.stop aborts fetch stream even before task event')
  assert.ok(button(root, 'Send'), 'P1-5: running state resets cleanly after stop')

  // Now stop AFTER task event when cancelTask throws 500 -> stream MUST still abort!
  let abortedAfterCancelErr = false
  handler = (path, _method, _body, init) => {
    if (path === '/api/v0/chat') {
      const enc = new TextEncoder()
      const stream = new ReadableStream({
        start(controller) {
          controller.enqueue(enc.encode('data: {"type":"session","session_id":"s1"}\n\n'))
          controller.enqueue(enc.encode('data: {"type":"task","task_id":"t-123"}\n\n'))
          init.signal?.addEventListener('abort', () => {
            abortedAfterCancelErr = true
            controller.error(new DOMException('Aborted', 'AbortError'))
          })
        },
      })
      return new Response(stream, { headers: { 'Content-Type': 'text/event-stream' } })
    }
    if (path === '/api/v0/session/cancel') {
      return { status: 500, body: { error: 'cancel endpoint unavailable' } }
    }
    return {}
  }
  edit(composer, 'hello with failing cancel')
  await nextTick()
  const sending2 = button(root, 'Send').props.onClick()
  await flush()
  await button(root, 'Stop').props.onClick()
  await sending2
  await flush()
  assert.equal(abortedAfterCancelErr, true, 'P1-5: stream still aborted when cancelTask rejects')
  app.unmount()

  // postChat trailing buffer flush without trailing newline
  const events: any[] = []
  handler = () =>
    new Response('data: {"type":"token","text":"first"}\ndata: {"type":"token","text":"tail-without-nl"}', {
      headers: { 'Content-Type': 'text/event-stream' },
    })
  await postChat(null, 'hi', (ev) => events.push(ev))
  assert.deepEqual(
    events.map((e) => e.text),
    ['first', 'tail-without-nl'],
    'P1-5: postChat flushes trailing buffer on EOF',
  )

  // postChat timeout throws ApiError('timeout', ...) instead of 'aborted'
  handler = (_p, _m, _b, init) =>
    new Promise((_resolve, reject) => {
      init.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')))
    })
  let caught: unknown = null
  try {
    await postChat(null, 'slow', () => {}, { timeoutMs: 15 })
  } catch (err) {
    caught = err
  }
  assert.ok(caught instanceof ApiError && caught.code === 'timeout', `expected timeout ApiError, got ${String(caught)}`)
}

// ── P1-6, P1-9, P1-10, P2-10, P2-12, P2-14, P2-16, P2-19 spot checks ──
{
  locale.value = 'en'

  // P1-6: LedCard disables Save when initial profile GET fails
  handler = () => ({ status: 500, body: { error: 'profile offline' } })
  const ledRoot = node('root')
  const ledApp = renderer.createApp(LedCard, {
    device: { device: 'dot2-a', assistant_state: 'idle', duplex: { capability: 'half', requested: 'half', negotiated: 'half', effective: 'half' }, participation: 'idle', proactive_speech: false, activity: 'idle', outputs: [] } as any,
    gatewayDevice: null,
  })
  ledApp.mount(ledRoot)
  await flush()
  assert.equal(button(ledRoot, 'Save night LED & orientation').props.disabled, true, 'P1-6: LedCard Save disabled on load failure')
  assert.ok(text(ledRoot).includes('profile offline'))
  ledApp.unmount()

  // P1-6 & P2-16: StoryJokeCard disables Save on load failure, and testAll uses && for tone
  handler = () => ({ status: 500, body: { error: 'media cfg unreachable' } })
  const sjRoot = node('root')
  const sjApp = renderer.createApp(StoryJokeCard)
  sjApp.mount(sjRoot)
  await flush()
  assert.equal(button(sjRoot, 'Save services').props.disabled, true, 'P1-6: StoryJokeCard Save disabled on load failure')
  sjApp.unmount()

  handler = (path, method) => {
    if (method === 'GET') return { enabled: true, url: 'http://x', timeout_ms: 4000, bearer_token_set: false, state: 'ready' }
    if (path.endsWith('/story/test')) return { ok: true, state: 'ready' }
    if (path.endsWith('/joke/test')) return { ok: false, state: 'failed', detail: 'bad auth' }
    if (path.endsWith('/radio/test')) return { ok: true, state: 'ready' }
    return {}
  }
  const sjRoot2 = node('root')
  const sjApp2 = renderer.createApp(StoryJokeCard)
  sjApp2.mount(sjRoot2)
  await flush()
  assert.equal(button(sjRoot2, 'Save services').props.disabled, false)
  await button(sjRoot2, 'Test connection').props.onClick()
  await flush()
  const statusSpan = all(sjRoot2).find((n) => n.props.role === 'status')
  assert.equal(statusSpan?.props.class, 'pm__error', 'P2-16: partial test failure must render error tone')
  sjApp2.unmount()

  // P1-9: ToneCard passes raw ApiError to ErrorNotice (real message, not Unknown error)
  handler = () => ({ status: 500, body: { error: 'eq backend unreachable' } })
  const toneRoot = node('root')
  const toneApp = renderer.createApp(ToneCard)
  toneApp.mount(toneRoot)
  await flush()
  assert.ok(text(toneRoot).includes('eq backend unreachable'), 'P1-9: ToneCard surfaces real error detail')
  assert.ok(!text(toneRoot).includes('Unknown error'))
  toneApp.unmount()

  // P1-10: VolumeCard no longer renders obsolete physical-button-only mute hint
  const volRoot = node('root')
  const volApp = renderer.createApp(VolumeCard, {
    device: { device: 'dot2-a', muted: false } as any,
    gatewayDeviceId: 'dot2-a',
    volume: 0.5,
  })
  volApp.mount(volRoot)
  await flush()
  assert.ok(!text(volRoot).includes('Mute only changes on the device'), 'P1-10: obsolete mute block removed')
  volApp.unmount()

  // P2-10: OpenSocketsCard resets method to GET when switching effect back to Query from DELETE
  handler = () => ({ sockets: [] })
  const osRoot = node('root')
  const osApp = renderer.createApp(OpenSocketsCard)
  osApp.mount(osRoot)
  await flush()
  await button(osRoot, 'Add socket').props.onClick()
  await nextTick()
  const selects = all(osRoot).filter((n) => n.tag === 'select')
  const effectSel = selects[0]
  const methodSel = selects[1]
  effectSel.props.onChange?.({ target: { value: 'Mutate' } })
  await nextTick()
  methodSel.props.onChange?.({ target: { value: 'DELETE' } })
  await nextTick()
  assert.equal(methodSel.value, 'DELETE')
  effectSel.props.onChange?.({ target: { value: 'Query' } })
  await nextTick()
  assert.equal(methodSel.value, 'GET', 'P2-10: switching back to Query resets DELETE -> GET')
  osApp.unmount()

  // P2-12: WizardServiceCard preserves llm.thinking
  calls = []
  handler = (path, method) => {
    if (method === 'PUT') return { state: 'applied', service: {} }
    if (path.endsWith('/test')) return { ok: true, latency_ms: 40, reply: 'hi' }
    return {}
  }
  const wizRoot = node('root')
  const wizApp = renderer.createApp(WizardServiceCard, {
    service: 'llm',
    status: {
      provider: 'deepseek',
      base_url: 'https://api.deepseek.com',
      model: 'deepseek-reasoner',
      api_key_env: 'DEEPSEEK_API_KEY',
      api_key_set: true,
      configured: true,
      thinking: 'disabled',
    },
  })
  wizApp.mount(wizRoot)
  await flush()
  await button(wizRoot, 'Save & test').props.onClick()
  await flush()
  const svcPut = calls.find((c) => c.path === '/api/v0/config/services/llm' && c.method === 'PUT')
  assert.equal(svcPut?.body?.thinking, 'disabled', 'P2-12: WizardServiceCard preserves llm.thinking')
  wizApp.unmount()

  // P2-14: DuplexCard keys reasons per device
  handler = (path, method) => {
    if (path === '/api/v0/mode/dot2-a' && method === 'PUT') return { reason: 'device-a half only' }
    return {}
  }
  const dupRoot = node('root')
  const dupApp = renderer.createApp(DuplexCard, {
    mode: {
      default_duplex: 'half',
      wake_words: [],
      devices: [
        { device: 'dot2-a', duplex: { capability: 'half', requested: 'half', negotiated: 'half', effective: 'half' }, participation: 'idle', proactive_speech: false, activity: 'idle', outputs: [] },
        { device: 'dot2-b', duplex: { capability: 'half', requested: 'half', negotiated: 'half', effective: 'half' }, participation: 'idle', proactive_speech: false, activity: 'idle', outputs: [] },
      ],
    } as any,
    refresh: async () => {},
  })
  dupApp.mount(dupRoot)
  await flush()
  const fullBtns = all(dupRoot).filter((n) => n.tag === 'button' && text(n).trim().includes('Request full duplex'))
  await fullBtns[0].props.onClick()
  await flush()
  const reasonNodes = all(dupRoot).filter((n) => n.props.class === 'duplex__reason')
  assert.equal(reasonNodes.length, 1, 'P2-14: reason only shown on the requested device row')
  assert.ok(text(reasonNodes[0]).includes('device-a half only'))
  dupApp.unmount()

  // P2-19: TextInput binds mono class on <input>
  const tiRoot = node('root')
  const tiApp = renderer.createApp(TextInput, { modelValue: 'abc', label: 'Addr', mono: true })
  tiApp.mount(tiRoot)
  await flush()
  const tiInp = all(tiRoot).find((n) => n.tag === 'input')
  assert.ok(String(tiInp.props.class).includes('mono'), 'P2-19: TextInput binds mono class')
  tiApp.unmount()

  // Field UX fix: AuthGate.pair() sanitizes surrounding quotes, backticks, newlines, whitespace, and Bearer prefix
  for (const pasted of [
    '"hm1_sanitized_token"\n',
    " 'hm1_sanitized_token' ",
    '`hm1_sanitized_token`',
    'Bearer hm1_sanitized_token',
    '  "Bearer hm1_sanitized_token"\r\n',
    "Bearer 'hm1_sanitized_token'",
    'hm1_sanitized_\ntoken',
  ]) {
    clearAuthToken()
    let readied = false
    let retried = false
    let statusAuthHeader = ''
    handler = (path) => {
      if (path === '/api/v0/auth') return { configured: true, corrupt: false, tokens: [], loopback_trusted: false }
      if (path === '/api/v0/status') {
        statusAuthHeader = authToken.value ?? ''
        return { product: 'home-mate', assistant_name: 'Home Mate', version: '1.0.0', services: {}, gateway: { connected: true, generation: 1, bind_addr: '127.0.0.1:8932', devices: [], pending_plays: 0 } }
      }
      return {}
    }
    const sanRoot = node('root')
    const sanApp = renderer.createApp(AuthGate, {
      state: 'pair',
      onReady: () => { readied = true },
      onRetry: () => { retried = true },
    })
    sanApp.mount(sanRoot)
    await flush()
    const pw = all(sanRoot).find((n) => n.tag === 'input' && n.props.type === 'password')
    edit(pw, pasted)
    await nextTick()
    await button(sanRoot, 'Pair').props.onClick()
    await flush()
    assert.equal(readied, true, `AuthGate.pair must accept sanitized paste: ${JSON.stringify(pasted)}`)
    assert.equal(retried, false)
    assert.equal(authToken.value, 'hm1_sanitized_token', `stored token must be clean for ${JSON.stringify(pasted)}`)
    assert.equal(statusAuthHeader, 'hm1_sanitized_token')
    sanApp.unmount()
  }

  // Empty after sanitization ('""', 'Bearer   ') keeps Pair button disabled
  {
    clearAuthToken()
    const emptyRoot = node('root')
    const emptyApp = renderer.createApp(AuthGate, { state: 'pair' })
    emptyApp.mount(emptyRoot)
    await flush()
    const pw = all(emptyRoot).find((n) => n.tag === 'input' && n.props.type === 'password')
    for (const junk of ['""', "''", '``', 'Bearer   ', ' " \n " ']) {
      edit(pw, junk)
      await nextTick()
      assert.equal(Boolean(button(emptyRoot, 'Pair').props.disabled), true, `Pair must stay disabled for ${JSON.stringify(junk)}`)
    }
    emptyApp.unmount()
  }

  // Issue #5 UX fix: ProviderCard uses fixed <select> options for ASR/TTS + contextual /v1 hints,
  // and preserves legacy/custom provider values; WizardServiceCard allows selecting omni TTS.
  {
    calls = []
    handler = (_path, method) => (method === 'PUT' ? { state: 'applied', service: {} } : { ok: true, latency_ms: 20 })

    // 1. ASR ProviderCard: select only exposes '', 'openai-asr', 'mock' and hints /v1
    const asrRoot = node('root')
    const asrApp = renderer.createApp(ProviderCard, {
      service: 'asr',
      title: 'Speech recognition (ASR)',
      status: { provider: 'unconfigured', base_url: '', model: '', api_key_env: '', api_key_set: false, configured: false },
      refresh: async () => {},
    })
    asrApp.mount(asrRoot)
    await flush()
    const asrSelect = all(asrRoot).find((n) => n.tag === 'select')
    assert.ok(asrSelect, 'ASR ProviderCard must render a <select> for provider')
    const asrOpts = asrSelect.options.map((o: any) => o.props.value)
    assert.deepEqual(asrOpts, ['', 'openai-asr', 'mock'])
    assert.ok(text(asrRoot).includes('/v1'), 'ASR ProviderCard must show /v1 base_url hint')
    asrSelect.props.onChange?.({ target: { value: 'openai-asr' } })
    await nextTick()
    await button(asrRoot, 'Save').props.onClick()
    await flush()
    const asrPut = calls.find((c) => c.path === '/api/v0/config/services/asr' && c.method === 'PUT')
    assert.equal(asrPut?.body?.provider, 'openai-asr')
    asrApp.unmount()

    // 2. TTS ProviderCard: exposes '', 'volc', 'omni', 'mock' AND preserves legacy 'openai_compat'
    const ttsRoot = node('root')
    const ttsApp = renderer.createApp(ProviderCard, {
      service: 'tts',
      title: 'Speech synthesis (TTS)',
      status: { provider: 'openai_compat', base_url: 'https://api.example.com/v1', model: 'tts-1', voice: 'alloy', api_key_env: '', api_key_set: true, configured: true },
      refresh: async () => {},
    })
    ttsApp.mount(ttsRoot)
    await flush()
    const ttsSelect = all(ttsRoot).find((n) => n.tag === 'select')
    assert.ok(ttsSelect, 'TTS ProviderCard must render a <select> for provider')
    assert.deepEqual(
      ttsSelect.options.map((o: any) => o.props.value),
      ['', 'volc', 'omni', 'mock', 'openai_compat'],
      'TTS ProviderCard must preserve existing openai_compat value alongside volc/omni/mock',
    )
    ttsApp.unmount()

    // 3. LLM ProviderCard keeps free-text <input> for provider (only select is thinkingMode)
    const llmRoot = node('root')
    const llmApp = renderer.createApp(ProviderCard, {
      service: 'llm',
      title: 'Conversation model (LLM)',
      status: { provider: 'deepseek', base_url: 'https://api.deepseek.com/v1', model: 'deepseek-chat', api_key_env: '', api_key_set: true, configured: true },
      refresh: async () => {},
    })
    llmApp.mount(llmRoot)
    await flush()
    const llmProviderInput = all(llmRoot).find((n) => n.tag === 'input' && n.props.placeholder === 'deepseek / openai / siliconflow')
    assert.ok(llmProviderInput, 'LLM ProviderCard must keep free-text input for OpenAI-compatible provider labels')
    llmApp.unmount()

    // 4. WizardServiceCard for TTS lets user switch volc -> omni and saves provider='omni'
    calls = []
    const wizTtsRoot = node('root')
    const wizTtsApp = renderer.createApp(WizardServiceCard, {
      service: 'tts',
      status: { provider: 'unconfigured', base_url: '', model: '', voice: '', api_key_env: '', api_key_set: false, configured: false },
    })
    wizTtsApp.mount(wizTtsRoot)
    await flush()
    const wizTtsSelect = all(wizTtsRoot).find((n) => n.tag === 'select')
    assert.ok(wizTtsSelect, 'WizardServiceCard (tts) must render a provider <select>')
    assert.deepEqual(wizTtsSelect.options.map((o: any) => o.props.value), ['volc', 'omni'])
    wizTtsSelect.props.onChange?.({ target: { value: 'omni' } })
    await nextTick()
    await button(wizTtsRoot, 'Save & test').props.onClick()
    await flush()
    const wizTtsPut = calls.find((c) => c.path === '/api/v0/config/services/tts' && c.method === 'PUT')
    assert.equal(wizTtsPut?.body?.provider, 'omni')
    wizTtsApp.unmount()
  }
}

console.log('Wave 5 audit regression suite (P0-1, P0-2, P1-1..P1-10, P2 spot checks) passed')
