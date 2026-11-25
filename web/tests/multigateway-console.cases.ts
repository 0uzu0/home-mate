import assert from 'node:assert/strict'
import { createRenderer, h, nextTick, reactive } from 'vue'
import { isDeviceIdleGated, resolveWifiProvisioningForDevice } from '../src/api'
import { locale } from '../src/i18n'
import * as devicesStore from '../src/stores/devices'
import MaintenanceCard from '../src/components/settings/MaintenanceCard.vue'
import ProbesCard from '../src/components/diagnostics/ProbesCard.vue'
import SelfCheckCard from '../src/components/diagnostics/SelfCheckCard.vue'
import ReminderCard from '../src/components/device/ReminderCard.vue'
import StatusBoard from '../src/components/status/StatusBoard.vue'
import OutputsCard from '../src/components/status/OutputsCard.vue'
import MicCard from '../src/components/device/MicCard.vue'
import AudioGateCard from '../src/components/device/AudioGateCard.vue'
import FollowUpPanel from '../src/components/interaction/FollowUpPanel.vue'
import InterruptStylePanel from '../src/components/interaction/InterruptStylePanel.vue'
import WakeResponsePanel from '../src/components/interaction/WakeResponsePanel.vue'
import ArbitrationCard from '../src/components/interaction/ArbitrationCard.vue'

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
          n.selectedIndex >= 0
            ? n.options[n.selectedIndex]
            : n.options.find((o: any) => o.selected)
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
    if (key === 'type' || key === 'value') el[key] = value
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
  assert.ok(found, `button "${label}" must render; got: ${all(root).filter((n) => n.tag === 'button').map((b) => text(b).trim()).join(' | ')}`)
  return found
}
function edit(el: any, value: unknown) {
  el.props['onUpdate:modelValue']?.(value)
  el.props.onInput?.({ target: { value } })
  el.props.onChange?.({ target: { value, checked: value } })
}
async function flush() {
  for (let i = 0; i < 14; i++) {
    await Promise.resolve()
    await nextTick()
  }
}

let calls: { path: string; method: string; body: any }[] = []
let handler: (path: string, method: string, body: any) => any = () => ({})
Object.assign(globalThis, {
  fetch: async (path: string, init: any = {}) => {
    const method = init.method ?? 'GET'
    const body = init.body ? JSON.parse(init.body) : null
    calls.push({ path, method, body })
    const result = await handler(path, method, body)
    const envelope = result && typeof result === 'object' && typeof result.status === 'number' && Object.hasOwn(result, 'body')
    return new Response(JSON.stringify(envelope ? result.body : result), {
      status: envelope ? result.status : 200,
      headers: { 'Content-Type': 'application/json' },
    })
  },
})

function makeDevice(id: string, overrides: Record<string, any> = {}): any {
  return {
    device: id,
    duplex: { capability: 'half', requested: 'half', negotiated: 'half', effective: 'half' },
    muted: false,
    participation: 'idle',
    proactive_speech: false,
    follow_window_tone: false,
    retreat_intent: true,
    activity: 'idle',
    assistant_state: 'idle',
    outputs: [],
    kws: { state: 'running', restarts: 0, detections: 0, guarded: 0, stops: 0, ignored: 0, last_keyword_ms: 0, phrase: '小智', core_epoch: 100 },
    mic: {
      session: true,
      in_speech: false,
      chunks_per_sec: 12.0,
      blocks_seen: 40,
      vad: { start_db: -38, end_db: -44 },
      counters: { speech_started: 0, segments: 0, segments_discarded: 0, wakes: 0, followup_turns: 0, ignored_unwoken: 0, asr_failed: 0, self_echo_suppressed: 0 },
    },
    ...overrides,
  }
}

// ── MG-01: MaintenanceCard uses selected device (not devices[0]) + selector ──
{
  locale.value = 'en'
  calls = []
  const devA = makeDevice('dot2-a', { kws: { state: 'running', restarts: 0, detections: 0, guarded: 0, stops: 0, ignored: 0, last_keyword_ms: 0, phrase: '小智', core_epoch: 111 } })
  const devB = makeDevice('dot2-b', { kws: { state: 'running', restarts: 0, detections: 0, guarded: 0, stops: 0, ignored: 0, last_keyword_ms: 0, phrase: '小智', core_epoch: 222 } })
  devicesStore.mode.value = { default_duplex: 'half', wake_words: ['小智'], devices: [devA, devB] }
  devicesStore.selectedId.value = 'dot2-b'

  let postRestart = false
  handler = (path, method) => {
    if (path.startsWith('/api/v0/mode')) {
      return {
        default_duplex: 'half',
        wake_words: ['小智'],
        devices: [
          { ...devA, kws: { ...devA.kws, core_epoch: 111 } }, // devices[0] unchanged!
          { ...devB, kws: { ...devB.kws, core_epoch: postRestart ? 333 : 222 } },
        ],
      }
    }
    if (path === '/api/v0/system/restart' && method === 'POST') {
      postRestart = true
      return { state: 'restarting' }
    }
    return {}
  }

  // Intercept setTimeout to step recovery quickly & deterministically
  const origSetTimeout = globalThis.window.setTimeout
  const scheduled: Array<() => void> = []
  ;(globalThis.window as any).setTimeout = (fn: () => void, ms?: number) => {
    if (ms === 1200) {
      scheduled.push(fn)
      return scheduled.length
    }
    return origSetTimeout(fn, ms)
  }

  const root = node('root')
  const app = renderer.createApp(MaintenanceCard)
  app.mount(root)
  await flush()

  const sel = all(root).find((n) => n.tag === 'select')
  assert.ok(sel, 'MaintenanceCard renders explicit device selector when >1 device')
  assert.equal(sel.value, 'dot2-b')

  // Manual <select> override must survive 500ms devicesStore.mode poll refreshes without snapping back
  sel.value = 'dot2-a'
  sel.listeners.change?.({ target: sel })
  await flush()
  assert.equal(sel.value, 'dot2-a', 'Manual select switches target device to dot2-a')
  devicesStore.mode.value = { default_duplex: 'half', wake_words: ['小智'], devices: [{ ...devA }, { ...devB }] }
  await flush()
  assert.equal(sel.value, 'dot2-a', '500ms mode poll must not snap manual select back to dot2-b')
  // Switch back to dot2-b to verify restart probes dot2-b's epoch (not devices[0])
  sel.value = 'dot2-b'
  sel.listeners.change?.({ target: sel })
  await flush()
  assert.equal(sel.value, 'dot2-b')

  await button(root, 'Restart core').props.onClick()
  await flush()
  const confirmBtn = all(teleportTarget).filter((n) => n.tag === 'button' && text(n).trim() === 'Restart core').at(-1)
  assert.ok(confirmBtn)
  await confirmBtn.props.onClick()
  await flush()

  // Attempt 0 schedules attempt 1; run both ticks and confirm recovery uses dot2-b epoch (222 -> 333)
  assert.ok(calls.some((c) => c.path === '/api/v0/mode?device=dot2-b'), 'MaintenanceCard must query selected device')
  while (scheduled.length) {
    const fn = scheduled.shift()!
    fn()
    await flush()
  }
  assert.ok(text(root).includes('The core restarted and is back'), 'Recovery confirmed from selected device epoch even when devices[0] epoch stayed unchanged')

  ;(globalThis.window as any).setTimeout = origSetTimeout
  app.unmount()
}

// ── MG-02: ProbesCard targets selected device for ALL 4 audible probes & surfaces DEVICE_REQUIRED ──
{
  locale.value = 'en'
  calls = []
  const devA = makeDevice('dot2-a')
  const devB = makeDevice('dot2-b')
  devicesStore.selectedId.value = 'dot2-b'

  handler = (path, _method, body) => {
    if (path === '/api/v0/diagnostics/tests/voice_play') {
      return { ok: true, device: body?.device, stage: 'device_confirmed', synth_ms: 120, total_ms: 340, provider: 'edge', voice: 'zh' }
    }
    if (path === '/api/v0/diagnostics/tests/play_asset') {
      return { ok: true, device: body?.device, asset: body?.asset, stage: 'device_confirmed', duration_ms: 1000, total_ms: 1200 }
    }
    if (path === '/api/v0/diagnostics/tests/playback_loopback') {
      return { ok: true, play: { ok: true, device: body?.device }, loopback: { verdict: 'loopback_intact', correlation: 0.99, lag_samples: 4, longest_zero_ms: 0 } }
    }
    if (path === '/api/v0/diagnostics/tests/mics') {
      return { ok: true, play: { ok: true, device: body?.device }, mics: { frames: 256, active_ms: 1200 } }
    }
    return { status: 404, body: { error: 'not found' } }
  }

  const root = node('root')
  const app = renderer.createApp(ProbesCard, { devices: [devA, devB], selectedDeviceId: 'dot2-b' })
  app.mount(root)
  await flush()

  const sel = all(root).find((n) => n.tag === 'select')
  assert.ok(sel, 'ProbesCard renders device selector')
  assert.equal(sel.value, 'dot2-b', 'ProbesCard defaults to selectedDeviceId, never devices[0]')

  await button(root, 'Voice-chain test').props.onClick()
  await flush()
  await button(root, 'Play fixed audio asset').props.onClick()
  await flush()
  await button(root, 'ch8 playback self-check').props.onClick()
  await flush()
  await button(root, '7-mic array measurement').props.onClick()
  await flush()

  assert.deepEqual(
    calls.map((c) => [c.path, c.body?.device]),
    [
      ['/api/v0/diagnostics/tests/voice_play', 'dot2-b'],
      ['/api/v0/diagnostics/tests/play_asset', 'dot2-b'],
      ['/api/v0/diagnostics/tests/playback_loopback', 'dot2-b'],
      ['/api/v0/diagnostics/tests/mics', 'dot2-b'],
    ],
  )

  // Verify backend 400 DEVICE_REQUIRED surfaces cleanly in EN and ZH
  handler = () => ({
    status: 400,
    body: { ok: false, code: 'DEVICE_REQUIRED', error: 'DEVICE_REQUIRED: multiple devices are online' },
  })
  await button(root, 'Voice-chain test').props.onClick()
  await flush()
  assert.ok(text(root).includes('select a target device explicitly'), 'EN DEVICE_REQUIRED message shown')

  locale.value = 'zh'
  await flush()
  await button(root, '语音链路测试').props.onClick()
  await flush()
  assert.ok(text(root).includes('请先明确选择目标设备'), 'ZH DEVICE_REQUIRED message shown')
  locale.value = 'en'
  await flush()
  app.unmount()
}

// ── MG-03: ReminderCard explicit target device picker + 400 DEVICE_REQUIRED handling ──
{
  locale.value = 'en'
  calls = []
  const devA = makeDevice('dot2-a')
  const devB = makeDevice('dot2-b')
  let returnDeviceRequired = false
  handler = (path, method, body) => {
    if (path === '/api/v0/reminders' && method === 'GET') {
      return { now_ms: 1000, reminders: [] }
    }
    if (path === '/api/v0/reminders' && method === 'POST') {
      if (returnDeviceRequired) {
        return { status: 400, body: { ok: false, code: 'DEVICE_REQUIRED', error: 'DEVICE_REQUIRED: specify device' } }
      }
      return { id: 1, device: body.device, due_ms: 2000, note: body.note, created_ms: 1000, fired_ms: null, cancelled_ms: null, state: 'pending' }
    }
    return {}
  }

  const root = node('root')
  const app = renderer.createApp(ReminderCard, { device: devB, devices: [devA, devB] })
  app.mount(root)
  await flush()

  const sel = all(root).find((n) => n.tag === 'select')
  assert.ok(sel, 'ReminderCard renders target device selector when >1 device')
  assert.equal(sel.value, 'dot2-b')

  const noteInput = all(root).find((n) => n.tag === 'input' && n.props.type === 'text')
  edit(noteInput, 'Turn off oven')
  await nextTick()
  await button(root, 'Add reminder').props.onClick()
  await flush()

  const post = calls.find((c) => c.path === '/api/v0/reminders' && c.method === 'POST')
  assert.equal(post?.body?.device, 'dot2-b', 'POST /api/v0/reminders passes selected device explicitly')

  returnDeviceRequired = true
  edit(noteInput, 'Check tea')
  await nextTick()
  await button(root, 'Add reminder').props.onClick()
  await flush()
  assert.ok(text(root).includes('select a target device explicitly'), 'ReminderCard surfaces DEVICE_REQUIRED cleanly')
  app.unmount()
}

// ── MG-04: StatusBoard & OutputsCard isolation (no sibling/global leakage) + idle_gated ──
{
  locale.value = 'en'
  const devA = makeDevice('dot2-a', {
    outputs: [{ play_id: 'p-a1', channel: 'media', stage: 'active', started_ms: 1000 }],
  })
  const devB = makeDevice('dot2-b', {
    outputs: [],
    kws: { state: 'running', restarts: 0, detections: 0, guarded: 0, stops: 0, ignored: 0, last_keyword_ms: 0, phrase: '小智' },
    mic: {
      session: true,
      in_speech: false,
      chunks_per_sec: 0,
      blocks_seen: 0,
      vad: { start_db: -38, end_db: -44 },
      counters: { speech_started: 0, segments: 0, segments_discarded: 0, wakes: 0, followup_turns: 0, ignored_unwoken: 0, asr_failed: 0, self_echo_suppressed: 0 },
    },
  })
  const gwDevA = {
    stable_id: 'dot2-a',
    model: 'dot2',
    firmware: '1.0',
    capabilities: ['audio_in', 'audio_out'],
    audio_profile: 'dot2',
    online: true,
    loopback: true,
    generation: 5,
    last_seq: 99,
  }
  const gwDevBOffline = {
    stable_id: 'dot2-b',
    model: 'android-panel',
    firmware: '2.0',
    capabilities: ['audio_in', 'audio_out', 'idle_gated_mic'],
    audio_profile: 'android-v2',
    profile_version: 'android-v2',
    online: false,
    loopback: false,
    generation: 3,
    last_seq: 12,
  }
  const gwSnap: any = {
    connected: true, // sibling dot2-a is connected!
    multi_gateway: true,
    generation: 5,
    gateway_version: 'gw-sibling-9.9',
    protocol_operating: 2,
    bind_addr: '127.0.0.1:8932',
    pending_plays: 4, // node-global plays on sibling dot2-a!
    last_link_error: { kind: 'protocol_error', reason: 'sibling handshake failed' },
    connections: [{ generation: 5, gateway: 'gw-a', gateway_version: 'gw-sibling-9.9', protocol_operating: 2, peer_addr: '127.0.0.1:50000', authenticated: true, devices: ['dot2-a'] }],
    devices: [gwDevA, gwDevBOffline],
  }

  // 1) Offline selected device dot2-b must NOT inherit sibling dot2-a connected / pending_plays / link_error / version
  const sbRoot = node('root')
  const sbApp = renderer.createApp(StatusBoard, { device: devB, gateway: gwSnap, gatewayDevice: gwDevBOffline })
  sbApp.mount(sbRoot)
  await flush()
  const sbText = text(sbRoot)
  assert.ok(sbText.includes('not connected'), 'Offline selected device must show not connected despite sibling online')
  assert.ok(!sbText.includes('gw-sibling-9.9'), 'Selected device must not show sibling gateway version')
  assert.ok(!sbText.includes('sibling handshake failed'), 'Selected device must not show sibling link error')
  assert.ok(sbText.includes('device outputs') && !sbText.includes('pending plays'), 'StatusBoard scopes outputs count to selected device')
  sbApp.unmount()

  const outRoot = node('root')
  const outApp = renderer.createApp(OutputsCard, { device: devB, gateway: gwSnap, gatewayDevice: gwDevBOffline, refresh: async () => {} })
  outApp.mount(outRoot)
  await flush()
  const outText = text(outRoot)
  assert.ok(outText.includes('This device (dot2-b)'), 'OutputsCard labels selected device scope')
  assert.ok(outText.includes('device outputs: 0'), 'OutputsCard reports 0 outputs for dot2-b (not global pending_plays=4)')
  assert.ok(outText.includes('not connected'), 'OutputsCard reports dot2-b disconnected despite sibling connected')
  outApp.unmount()

  // 2) Online idle-gated device renders healthy "Idle-gated (offline KWS)" / "空闲门控（离线唤醒）" in StatusBoard, MicCard, and SelfCheckCard
  const gwDevBOnline = { ...gwDevBOffline, online: true }
  const gwSnapOnline = {
    ...gwSnap,
    connections: [
      ...gwSnap.connections,
      { generation: 6, gateway: 'android-gw', gateway_version: '2.1', protocol_operating: 2, peer_addr: '192.168.1.50:40000', authenticated: true, devices: ['dot2-b'] },
    ],
    devices: [gwDevA, gwDevBOnline],
  }
  const sbRoot2 = node('root')
  const sbApp2 = renderer.createApp(StatusBoard, { device: devB, gateway: gwSnapOnline, gatewayDevice: gwDevBOnline })
  sbApp2.mount(sbRoot2)
  await flush()
  assert.ok(text(sbRoot2).includes('Idle-gated (offline KWS)'), 'StatusBoard renders EN idle-gated badge')
  locale.value = 'zh'
  await flush()
  assert.ok(text(sbRoot2).includes('空闲门控（离线唤醒）'), 'StatusBoard renders ZH idle-gated badge')
  locale.value = 'en'
  sbApp2.unmount()

  const micRoot = node('root')
  const micApp = renderer.createApp(MicCard, { device: devB, gatewayDevice: gwDevBOnline })
  micApp.mount(micRoot)
  await flush()
  assert.ok(text(micRoot).includes('Idle-gated (offline KWS)'), 'MicCard renders healthy idle-gated badge')
  micApp.unmount()

  // 2b) Negative case: non-loopback fixed continuous speaker (e.g. LAN Dot2 / LX06) must NEVER be inferred as idle-gated
  const fixedLanDev = makeDevice('lx06-lan', {
    kws: { state: 'running', restarts: 0, detections: 0, guarded: 0, stops: 0, ignored: 0, last_keyword_ms: 0, phrase: '小智' },
    mic: {
      session: true,
      in_speech: false,
      chunks_per_sec: 0,
      blocks_seen: 0,
      vad: { start_db: -38, end_db: -44 },
      counters: { speech_started: 0, segments: 0, segments_discarded: 0, wakes: 0, followup_turns: 0, ignored_unwoken: 0, asr_failed: 0, self_echo_suppressed: 0 },
    },
  })
  const fixedLanGwDev: any = {
    stable_id: 'lx06-lan',
    model: 'dot2',
    firmware: '1.0',
    capabilities: ['audio_in', 'audio_out'],
    audio_profile: 'dot2',
    profile_version: 'dot2-array-postaec-v1',
    online: true,
    loopback: false,
    generation: 8,
    last_seq: 42,
  }
  assert.equal(
    isDeviceIdleGated(fixedLanDev, fixedLanGwDev),
    false,
    'Non-loopback fixed continuous speaker without idle_gated capability or mobile profile must NOT be idle-gated',
  )
  const sbLanRoot = node('root')
  const sbLanApp = renderer.createApp(StatusBoard, {
    device: fixedLanDev,
    gateway: {
      ...gwSnapOnline,
      connections: [{ generation: 8, gateway: 'lan-dot2', protocol_operating: 2, peer_addr: '192.168.1.90:40000', authenticated: true, devices: ['lx06-lan'] }],
      devices: [fixedLanGwDev],
    },
    gatewayDevice: fixedLanGwDev,
  })
  sbLanApp.mount(sbLanRoot)
  await flush()
  assert.ok(!text(sbLanRoot).includes('Idle-gated (offline KWS)'), 'Fixed LAN continuous speaker must not show idle-gated badge')
  sbLanApp.unmount()

  // SelfCheckCard renders idle_gated badge and passes selected device
  calls = []
  handler = (path, _method, body) => {
    if (path === '/api/v0/diagnostics/self_check') {
      assert.equal(body.device, 'dot2-b')
      return {
        verdict: 'ready',
        verdict_stage: null,
        deep: false,
        generated_ms: 1000,
        note: 'passive',
        stages: [
          {
            stage: 'mic',
            state: 'ready',
            detail: 'dot2-b 在线，离线唤醒待命中（空闲门控暂停上行音频）',
            evidence: {
              device: 'dot2-b',
              idle_gated: true,
              all_devices: [
                { device: 'dot2-a', state: 'ready', idle_gated: false },
                { device: 'dot2-b', state: 'ready', idle_gated: true },
              ],
            },
          },
        ],
      }
    }
    return {}
  }
  const scRoot = node('root')
  const scApp = renderer.createApp(SelfCheckCard, { devices: [devA, devB], selectedDeviceId: 'dot2-b' })
  scApp.mount(scRoot)
  await flush()
  await button(scRoot, 'Quick check').props.onClick()
  await flush()
  assert.ok(text(scRoot).includes('Idle-gated (offline KWS)'), 'SelfCheckCard shows idle-gated badge in EN')
  locale.value = 'zh'
  await flush()
  assert.ok(text(scRoot).includes('空闲门控（离线唤醒）'), 'SelfCheckCard shows idle-gated badge in ZH')
  locale.value = 'en'
  scApp.unmount()

  // 3) wifi_provisioning_by_device isolation (no sibling bleed)
  const statusWithSiblingWifi: any = {
    gateway: { devices: [gwDevA, gwDevBOnline] },
    wifi_provisioning: { requested: true, device: 'dot2-a', trigger: 'mute_hold', ts_ms: 1234 },
    wifi_provisioning_by_device: {
      'dot2-a': { requested: true, device: 'dot2-a', trigger: 'mute_hold', ts_ms: 1234 },
    },
  }
  assert.equal(resolveWifiProvisioningForDevice('dot2-b', statusWithSiblingWifi, null, 2).requested, false, 'dot2-b must not see sibling dot2-a wifi provisioning')
  assert.equal(resolveWifiProvisioningForDevice('dot2-a', statusWithSiblingWifi, null, 2).requested, true, 'dot2-a sees its own wifi provisioning')
  // Legacy single-device fallback without map
  const legacySingle: any = {
    gateway: { devices: [gwDevA] },
    wifi_provisioning: { requested: true, device: null, trigger: 'mute_hold', ts_ms: 1234 },
  }
  assert.equal(resolveWifiProvisioningForDevice('dot2-a', legacySingle, null, 1).requested, true, 'Legacy single-device fallback works')

  // Store-level wifiProvisioning tracks selected device strictly
  devicesStore.setStatusSnapshot(statusWithSiblingWifi)
  devicesStore.mode.value = { default_duplex: 'half', wake_words: ['小智'], devices: [devA, devB] }
  devicesStore.selectedId.value = 'dot2-b'
  assert.equal(devicesStore.wifiProvisioning.value.requested, false)
  devicesStore.selectedId.value = 'dot2-a'
  assert.equal(devicesStore.wifiProvisioning.value.requested, true)
  assert.equal(devicesStore.wifiProvisioning.value.device, 'dot2-a')
  devicesStore.setStatusSnapshot(null)
}

// ── MG-05: Per-device vs node-global mode reads/writes & bilingual scope badges ──
{
  locale.value = 'en'
  calls = []

  // Store refresh() passes ?device=selectedId to GET /api/v0/mode
  devicesStore.selectedId.value = 'dot2-b'
  handler = (path) => {
    if (path.startsWith('/api/v0/mode')) {
      return {
        scope: 'device',
        device: 'dot2-b',
        default_duplex: 'half',
        wake_words: ['小贝'],
        devices: [makeDevice('dot2-a'), makeDevice('dot2-b')],
      }
    }
    if (path === '/api/v0/devices') return { gateway: { connected: true, generation: 1, bind_addr: '127.0.0.1:8932', devices: [], pending_plays: 0 }, recent: [], voice_log: [] }
    if (path === '/api/v0/device-meta') return { meta: {} }
    return {}
  }
  await devicesStore.refresh()
  assert.ok(calls.some((c) => c.path === '/api/v0/mode?device=dot2-b' && c.method === 'GET'), 'store refresh must query GET /api/v0/mode?device=dot2-b')

  // AudioGateCard reads device-specific tail_blind_ms and saves to PUT /api/v0/mode/{dev}
  calls = []
  const devA = makeDevice('dot2-a', { tail_blind_ms: 400, media_trim_db: -6 })
  const devB = makeDevice('dot2-b', { tail_blind_ms: 950, media_trim_db: -3 })
  const modeSnap: any = {
    default_duplex: 'half',
    tail_blind_ms: 400,
    media_trim_db: -6,
    wake_words: ['小智'],
    retreat_words: ['退下'],
    retreat_ack: '好嘞',
    window_close_ack: '拜拜啦',
    truncation_notice: '说继续听后面',
    follow_window_enabled: true,
    follow_window_ms: 9000,
    context_window_ms: 240000,
    follow_window_tone: false,
    keyword_interrupt: true,
    stop_intents: { enabled: true, stop_words: ['停'], continue_words: ['继续'] },
    spoken: { max_sentences: 2, max_chars: 120, detail_max_sentences: 6, detail_max_chars: 300, backstop_max_sentences: 400, backstop_max_chars: 2000, continue_hint: '继续', resume_lead: '' },
    filler: { delay_ms: 300, tool_delay_ms: 600, mode: 'radio', thinking: ['嗯'], search: [], device: [], story: [] },
    wake_arbitration: { mode: 'off', peers: 1 },
    devices: [devA, devB],
  }
  handler = (path, method, body) => {
    if (path === '/api/v0/mode/dot2-b' && method === 'PUT') return { ...devB, ...body }
    if (path === '/api/v0/mode' && method === 'PUT') {
      Object.assign(modeSnap, body)
      return modeSnap
    }
    if (path.startsWith('/api/v0/mode') && method === 'GET') {
      return { ...modeSnap, scope: 'device', device: 'dot2-b', follow_window_ms: 15000, follow_window_enabled: true, context_window_ms: 240000, follow_window_tone: false }
    }
    if (path === '/api/v0/reflex' && method === 'GET') return { words: [{ text: '暂停', tokens: 'z àn t íng', intent: 'pause', enabled: true }], compiled: '', hash: 'abc', warnings: [] }
    if (path === '/api/v0/reflex' && method === 'PUT') return { state: 'saved', words: body.words, compiled: '', hash: 'abc' }
    if (path.includes('/conversation')) return { session_id: null, messages: [] }
    if (path === '/api/v0/fleet') return { self_id: 'core', self_id_source: 'default', push_actor: 'fleet', nodes: [] }
    if (path.startsWith('/api/v0/voice/events')) return { events: [] }
    return {}
  }

  const agProps = reactive<any>({ mode: modeSnap, device: devB, refresh: async () => {} })
  const agRoot = node('root')
  const agApp = renderer.createApp({ setup: () => () => h(AudioGateCard, agProps) })
  agApp.mount(agRoot)
  await flush()
  assert.ok(text(agRoot).includes('This device (dot2-b)'), 'AudioGateCard displays device scope badge')
  assert.ok(text(agRoot).includes('950 ms'), 'AudioGateCard displays dot2-b tail_blind_ms (950), not global (400)')
  await button(agRoot, 'Save window').props.onClick()
  await flush()
  const agPut = calls.find((c) => c.path === '/api/v0/mode/dot2-b' && c.method === 'PUT')
  assert.equal(agPut?.body?.tail_blind_ms, 950, 'AudioGateCard saves tail_blind_ms to PUT /api/v0/mode/dot2-b')
  // Switching selected device refreshes displayed tail_blind_ms to dot2-a
  agProps.device = devA
  await flush()
  assert.ok(text(agRoot).includes('400 ms') && text(agRoot).includes('This device (dot2-a)'))
  agApp.unmount()

  // FollowUpPanel: device fields -> PUT /api/v0/mode/dot2-b + readback GET /api/v0/mode?device=dot2-b; global ack -> PUT /api/v0/mode
  calls = []
  const devBFollow = makeDevice('dot2-b', {
    follow_window_enabled: true,
    follow_window_ms: 15000,
    context_window_ms: 240000,
    follow_window_tone: false,
    retreat_words: ['先退下'],
  })
  const fpRoot = node('root')
  const fpApp = renderer.createApp(FollowUpPanel, { mode: modeSnap, device: devBFollow, refresh: async () => {} })
  fpApp.mount(fpRoot)
  await flush()
  assert.ok(text(fpRoot).includes('This device (dot2-b)'), 'FollowUpPanel displays per-device badge')
  assert.ok(text(fpRoot).includes('Node-wide'), 'FollowUpPanel displays node-wide badge')
  // Save without editing global acks: MUST only hit PUT /api/v0/mode/dot2-b, NOT PUT /api/v0/mode
  await button(fpRoot, 'Save conversation window').props.onClick()
  await flush()
  const devPut = calls.find((c) => c.path === '/api/v0/mode/dot2-b' && c.method === 'PUT')
  assert.equal(devPut?.body?.follow_window_ms, 15000, 'FollowUpPanel writes device follow_window_ms to PUT /api/v0/mode/dot2-b')
  assert.deepEqual(devPut?.body?.retreat_words, ['先退下'])
  assert.equal(calls.some((c) => c.path === '/api/v0/mode' && c.method === 'PUT'), false, 'Unchanged global acks must not trigger PUT /api/v0/mode')
  assert.ok(calls.some((c) => c.path === '/api/v0/mode?device=dot2-b' && c.method === 'GET'), 'FollowUpPanel readback queries GET /api/v0/mode?device=dot2-b')

  // Now edit retreat_ack -> MUST write retreat_ack to global PUT /api/v0/mode
  calls = []
  const retreatAckInput = all(fpRoot).find((n) => n.tag === 'input' && n.value === '好嘞')
  assert.ok(retreatAckInput, 'retreat_ack input rendered')
  edit(retreatAckInput, '好的再见')
  await nextTick()
  await button(fpRoot, 'Save conversation window').props.onClick()
  await flush()
  const globalPut = calls.find((c) => c.path === '/api/v0/mode' && c.method === 'PUT')
  assert.equal(globalPut?.body?.retreat_ack, '好的再见', 'Edited global retreat_ack writes to PUT /api/v0/mode')
  assert.equal('retreat_ack' in (calls.find((c) => c.path === '/api/v0/mode/dot2-b' && c.method === 'PUT')?.body ?? {}), false, 'Global retreat_ack is not mixed into device PUT')
  fpApp.unmount()

  // InterruptStylePanel: splits keyword_interrupt (device PUT) vs stop_intents/spoken/filler/truncation (global PUT) + ZH badges
  calls = []
  const devBInt = makeDevice('dot2-b', { keyword_interrupt: false })
  const isRoot = node('root')
  const isApp = renderer.createApp(InterruptStylePanel, { mode: modeSnap, device: devBInt, refresh: async () => {} })
  isApp.mount(isRoot)
  await flush()
  const isText = text(isRoot)
  assert.ok(isText.includes('Device-scoped interrupt switch') && isText.includes('This device (dot2-b)'), 'InterruptStylePanel labels keyword_interrupt as device-scoped')
  assert.ok(isText.includes('Node-wide') && isText.includes('Node-wide vocabulary table; “Save & sync device” deploys it to dot2-b.'), 'InterruptStylePanel labels reflex vocabulary table as node-wide')
  await button(isRoot, 'Save & sync device').props.onClick()
  await flush()
  assert.equal(calls.find((c) => c.path === '/api/v0/mode/dot2-b' && c.method === 'PUT')?.body?.keyword_interrupt, false)
  assert.ok(calls.find((c) => c.path === '/api/v0/mode' && c.method === 'PUT')?.body?.stop_intents, 'stop_intents saved to global PUT /api/v0/mode')

  calls = []
  await button(isRoot, 'Save reply style & cues').props.onClick()
  await flush()
  assert.equal(calls.some((c) => c.path === '/api/v0/mode/dot2-b' && c.method === 'PUT'), false, 'Reply style/filler/truncation must NOT write to per-device PUT')
  const styleGlobalPut = calls.find((c) => c.path === '/api/v0/mode' && c.method === 'PUT')
  assert.ok(styleGlobalPut?.body?.spoken && styleGlobalPut?.body?.filler, 'Reply style/filler/truncation saved to global PUT /api/v0/mode')

  // Verify ZH scope badges render cleanly
  locale.value = 'zh'
  await flush()
  const isTextZh = text(isRoot)
  assert.ok(isTextZh.includes('设备端打断开关') && isTextZh.includes('当前设备 (dot2-b)') && isTextZh.includes('节点全局'), 'ZH scope badges render cleanly')
  assert.ok(isTextZh.includes('节点全局快捷指令词表；点「保存并同步设备」将同步部署到 dot2-b。'), 'ZH reflex node-wide hint renders cleanly')
  locale.value = 'en'
  await flush()
  isApp.unmount()

  // WakeResponsePanel & ArbitrationCard read per-device overrides (including asr_wake_fallback and
  // per-device follow_window_ms) and write to PUT /api/v0/mode/dot2-b
  calls = []
  const devBWake = makeDevice('dot2-b', {
    wake_words: ['客厅小智'],
    kws_threshold: 0.18,
    kws_score: 3.5,
    asr_wake_fallback: false,
    follow_window_ms: 15000,
    wake_arbitration: 'enforce',
  })
  const wrRoot = node('root')
  const wrApp = renderer.createApp(WakeResponsePanel, { mode: modeSnap, device: devBWake, refresh: async () => {} })
  wrApp.mount(wrRoot)
  await flush()
  const wrText = text(wrRoot)
  assert.ok(wrText.includes('This device (dot2-b)') && wrText.includes('客厅小智'))
  assert.ok(wrText.includes('15 s continuous window'), 'WakeResponsePanel reads per-device follow_window_ms (15 s, not global 9 s)')
  await button(wrRoot, 'Save & apply to speaker').props.onClick()
  await flush()
  const wakePut = calls.find((c) => c.path === '/api/v0/mode/dot2-b' && c.method === 'PUT')
  assert.deepEqual(wakePut?.body?.wake_words, ['客厅小智'])
  assert.equal(wakePut?.body?.kws_threshold, 0.18)
  assert.equal(wakePut?.body?.asr_wake_fallback, false)
  wrApp.unmount()

  calls = []
  const arbRoot = node('root')
  const arbApp = renderer.createApp(ArbitrationCard, { mode: modeSnap, device: devBWake, refresh: async () => {} })
  arbApp.mount(arbRoot)
  await flush()
  assert.ok(text(arbRoot).includes('This device (dot2-b)'))
  await button(arbRoot, 'Save setting').props.onClick()
  await flush()
  assert.equal(calls.find((c) => c.path === '/api/v0/mode/dot2-b' && c.method === 'PUT')?.body?.wake_arbitration, 'enforce')
  arbApp.unmount()
}

console.log('MG-01..05: multi-gateway console isolation, explicit device routing, idle_gated, wifi_provisioning_by_device, and per-device vs node-global mode checks passed')
