import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { createRenderer, h, reactive, nextTick } from 'vue'
import SnapshotCard from '../src/components/diagnostics/SnapshotCard.vue'
import { getDiagnostics } from '../src/api'
import { locale } from '../src/i18n'

function node(tag = '', text = ''): any { return { tag, text, children: [], parent: null, props: {},
  addEventListener() {}, removeEventListener() {}, setAttribute() {}, removeAttribute() {}, getRootNode() { return document } } }
const renderer = createRenderer<any, any>({
  createElement: tag => node(tag), createText: text => node('#text', text), createComment: text => node('#comment', text),
  insert(el, parent, anchor) { el.parent = parent; const at = parent.children.indexOf(anchor); if (at < 0) parent.children.push(el); else parent.children.splice(at, 0, el) },
  remove(el) { if (el.parent) el.parent.children.splice(el.parent.children.indexOf(el), 1) },
  setText(el, text) { el.text = text }, setElementText(el, text) { el.text = text; el.children = [] },
  parentNode: el => el.parent, nextSibling: el => el.parent?.children[el.parent.children.indexOf(el) + 1] ?? null,
  patchProp(el, key, _old, value) { el.props[key] = value },
})
const all = (root: any): any[] => [root, ...root.children.flatMap(all)]
const text = (root: any): string => root.tag === '#comment' ? '' : root.text + root.children.map(text).join('')
async function flush() { for (let i = 0; i < 12; i++) { await Promise.resolve(); await nextTick() } }
const sample = JSON.parse(await readFile('../docs/fixtures/northbound-status-v1.json', 'utf8')).northbound
let response: any = { services: {}, devices: [], voice_counts: {}, gateway_counts: {}, recent_errors: [], failed_traces: [], note: '',
  gateway: { connected: true, generation: 7, pending_plays: 0, bind_addr: '127.0.0.1:0', devices: [{
    stable_id: 'fixture-dot', model: 'fixture', capabilities: [], audio_profile: 'fixture', online: true, generation: 7, last_seq: 0,
    northbound: { state: 'available', schema: 1, sample, received_ms: 123456, connection_generation: 7, stale: false },
  }] } }
let requests = 0
Object.assign(globalThis, { fetch: async (path: string) => {
  assert.equal(path, '/api/v0/diagnostics'); requests++
  return new Response(JSON.stringify(response), { headers: { 'Content-Type': 'application/json' } })
} })
const props = reactive<any>({ data: null, error: null, loading: true, refresh: async () => { props.data = await getDiagnostics() } })
const root = node('root'); const app = renderer.createApp({ setup: () => () => h(SnapshotCard, props) }); app.mount(root)
props.data = await getDiagnostics(); props.loading = false; await flush()
assert.equal(requests, 1)
assert.ok(text(root).includes('Last report on this connection; not a live queue measurement.'))
assert.ok(text(root).includes('3 / 9') && text(root).includes('2048 / 8192'))
assert.ok(text(root).includes('42 / 17') && text(root).includes('13 / 25'))
assert.ok(text(root).includes('critical_count_full') && text(root).includes('Received by Core'))

async function updateReport(report: any, online = true) {
  response.gateway.devices[0].northbound = report
  response.gateway.devices[0].online = online
  props.data = await getDiagnostics(); await flush()
}
const reported = structuredClone(response.gateway.devices[0].northbound)
await updateReport(undefined)
assert.ok(text(root).includes('has not reported transport diagnostics'))
assert.ok(!text(root).includes('Critical frames now / high-water'), 'absent source cannot invent zero queue statistics')
await updateReport({ ...reported, state: 'unsupported', schema: 2, sample: null })
assert.ok(text(root).includes('unsupported transport diagnostic schema'))
assert.ok(!text(root).includes('Critical frames now / high-water'))
await updateReport({ ...reported, sample: { ...sample, last_fault: {} } })
assert.ok(text(root).includes('incomplete or invalid transport diagnostics'))
assert.ok(!text(root).includes('No failure reported in this snapshot'), 'missing fault fields cannot mean healthy')
const incomplete = { ...sample }; delete incomplete.critical_current_count
await updateReport({ ...reported, sample: incomplete })
assert.ok(text(root).includes('incomplete or invalid transport diagnostics'))
assert.ok(!text(root).includes('Critical frames now / high-water'))
await updateReport({ ...reported, stale: true }, false)
assert.ok(text(root).includes('retained last report; current queue and unreported failures are unknown'))
assert.ok(text(root).includes('2048 / 8192'), 'stale historical evidence remains visible as historical')
await updateReport({ ...reported, connection_generation: 6 })
assert.ok(text(root).includes('Stale/disconnected'), 'mismatched source generation cannot be current')
await updateReport(reported)
props.error = new Error('synthetic management refresh failed'); await flush()
assert.ok(text(root).includes('Stale/disconnected') && text(root).includes('synthetic management refresh failed'))
locale.value = 'zh'; await flush()
assert.ok(text(root).includes('当前队列和未送达的故障未知') && text(root).includes('编码字节当前值 / 高水位'))
props.error = null; await updateReport({ ...reported, state: 'invalid', sample: null })
assert.ok(text(root).includes('传输诊断不完整或无效'))
await updateReport(undefined)
assert.ok(text(root).includes('尚未报告传输诊断'))
props.error = new Error('synthetic refresh failure'); props.data = null; await flush()
assert.ok(text(root).includes('synthetic refresh failure'))
assert.ok(!text(root).includes('fixture_boot_1'), 'no snapshot must not look like retained data')
app.unmount(); locale.value = 'en'
console.log('Northbound snapshot: real SFC/API client last-report, absent, unsupported, malformed, stale, generation, refresh-error and bilingual cases passed')
