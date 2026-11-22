/** Actual production entry + dist, fresh synthetic data and browser-only pairing. */
import { test as base, expect, type Page, type Locator, type TestInfo } from '@playwright/test'
import { spawn, execFileSync, type ChildProcess } from 'node:child_process'
import { createHash } from 'node:crypto'
import { openSync, closeSync, existsSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, readdir, writeFile, rm } from 'node:fs/promises'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { resolve, join } from 'node:path'
import enConfig from '../../src/i18n/en/config'
import zhConfig from '../../src/i18n/zh/config'
import enSettings from '../../src/i18n/en/settings'
import zhSettings from '../../src/i18n/zh/settings'

const root = resolve(import.meta.dirname, '../..')
const hash = (data: Buffer | string) => createHash('sha256').update(data).digest('hex')
const sourceInputs = ['package.json', 'package-lock.json', 'playwright.config.ts',
  'tests/browser/core-fixture.ts', 'tests/browser/config-plan.spec.ts', 'tests/browser/ui-lifecycle.spec.ts',
  '../.github/workflows/ci.yml', '../docs/commercial-licensing.md']
async function sourceHashes() {
  return Object.fromEntries(await Promise.all(sourceInputs.map(async path => [path, hash(await readFile(join(root, path)))])))
}
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))
async function ports(): Promise<number[]> {
  const servers = [createServer(), createServer()]
  try {
    await Promise.all(servers.map(server => new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve))))
    return servers.map(server => (server.address() as { port: number }).port)
  } finally {
    await Promise.all(servers.map(server => new Promise<void>(resolve => server.close(() => resolve()))))
  }
}

export class RealCore {
  origin = ''
  token = '' // Ephemeral fixture bearer in memory; never an artifact or storageState.
  child!: ChildProcess
  data = ''
  artifact = ''
  observations: Record<string, any> = {}
  traffic: { method: string; path: string; status: number }[] = []
  assets = new Set<string>()
  failures: string[] = []
  allowInvalidPreflight = false
  allowedFailurePaths = new Set<string>()
  closing = false
  abortedReads: { path: string; error: string; seq: number; cause: string; replacement_seq?: number; successor?: unknown }[] = []
  requests: { seq: number; path: string; method: string; status?: number; finished?: boolean }[] = []
  requestMeta = new Map<object, typeof this.requests[number]>()
  reloadRequests = new Set<object>()
  labels!: typeof enConfig & typeof enSettings

  async start(info: TestInfo) {
    const binary = resolve(process.env.HM_CORE_BINARY || join(root, '../target/debug/hm-core'))
    const dist = resolve(root, 'dist')
    if (!existsSync(binary) || !existsSync(join(dist, 'index.html'))) throw new Error('Build actual hm-core and web/dist before browser acceptance')
    const evidence = resolve(process.env.HM_BROWSER_EVIDENCE_DIR || join(tmpdir(), 'hm-browser-evidence'))
    await mkdir(evidence, { recursive: true })
    this.artifact = await mkdtemp(join(evidence, info.project.name + '-'))
    // DB, generated secret.key and temporary household bearer stay outside
    // the evidence/upload directory and are removed only after child exit.
    this.data = await mkdtemp(join(tmpdir(), 'hm-browser-data-'))
    const [apiPort, gatewayPort] = await ports()
    this.origin = `http://127.0.0.1:${apiPort}`
    const log = openSync(join(this.artifact, 'core.log'), 'wx')
    this.child = spawn(binary, [], {
      cwd: this.data,
      env: { PATH: '/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin', HM_DATA_DIR: this.data,
        HM_NODE_ID: `browser-fixture-${info.project.name}`, HM_BIND: `127.0.0.1:${apiPort}`,
        HM_GATEWAY_BIND: `127.0.0.1:${gatewayPort}`, HM_WEB_DIR: dist, RUST_LOG: 'hm_core=info' },
      stdio: ['ignore', log, log],
    })
    closeSync(log)
    this.child.on('error', error => this.failures.push('Core spawn: ' + error.message))
    const until = Date.now() + 15_000
    let status: any
    while (Date.now() < until) {
      if (this.child.exitCode !== null) throw new Error('Actual Core exited during startup')
      try {
        const response = await fetch(this.origin + '/api/v0/status', { signal: AbortSignal.timeout(1000) })
        if (response.ok) { status = await response.json(); break }
      } catch { /* Bounded readiness while the actual listener starts. */ }
      await pause(50)
    }
    if (!status) throw new Error('Actual Core startup exceeded15s')
    expect(status.data_dir).toBe(this.data)
    expect(status.gateway.connected).toBe(false)
    expect(status.gateway.devices).toEqual([])
    const files = ['index.html', ...(await readdir(join(dist, 'assets'))).map(name => 'assets/' + name)]
    const assets = Object.fromEntries(await Promise.all(files.map(async name => [name, hash(await readFile(join(dist, name)))])))
    this.observations.manifest = {
      source: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim(),
      tree: execFileSync('git', ['rev-parse', 'HEAD^{tree}'], { cwd: root, encoding: 'utf8' }).trim(),
      source_inputs: await sourceHashes(),
      binary, binary_sha256: hash(await readFile(binary)), build: status.build,
      dist, assets, assets_sha256: hash(JSON.stringify(assets)), node: process.version,
      playwright: JSON.parse(await readFile(join(root, 'node_modules/@playwright/test/package.json'), 'utf8')).version,
      pid: this.child.pid, data_dir: this.data, gateway_devices: [],
      scope: ['real-browser', 'actual-Core-main', 'actual-config-worker', 'synthetic-only'],
    }
  }

  watch(page: Page) {
    page.on('pageerror', error => this.failures.push('pageerror: ' + error.message))
    page.on('requestfailed', request => {
      if (this.closing) return
      const path = new URL(request.url()).pathname
      const error = request.failure()?.errorText || 'unknown'
      // A timeout abort followed by a future retry is not a replacement.
      // Prove the cause at cancellation, then verify its successful consumer.
      const meta = this.requestMeta.get(request)
      if (meta && request.method() === 'GET' && path.startsWith('/api/') && error === 'net::ERR_ABORTED') {
        const replacement = this.requests.find(value => value.seq > meta.seq && value.path === path && value.method === 'GET')
        const reloaded = this.reloadRequests.has(request)
        if (!reloaded && !replacement) this.failures.push(`API abort lacks a replacement at cancellation: ${path} #${meta.seq}`)
        this.abortedReads.push({ path, error, seq: meta.seq,
          cause: reloaded ? 'explicit_hard_reload' : replacement ? 'superseded_by_existing_resource_read' : 'unproven_abort',
          replacement_seq: replacement?.seq })
      } else this.failures.push(`requestfailed: ${path}: ${error}`)
    })
    page.on('request', request => {
      const url = new URL(request.url())
      const meta = { seq: this.requests.length + 1, path: url.pathname, method: request.method() }
      this.requests.push(meta)
      this.requestMeta.set(request, meta)
      if (['http:', 'https:'].includes(url.protocol) && url.origin !== this.origin) this.failures.push('external application request: ' + url.origin + url.pathname)
    })
    page.on('requestfinished', request => {
      const meta = this.requestMeta.get(request)
      if (meta) meta.finished = true
    })
    page.on('response', response => {
      const path = new URL(response.url()).pathname
      const meta = this.requestMeta.get(response.request())
      if (meta) meta.status = response.status()
      if (path.startsWith('/assets/')) this.assets.add(path)
      this.traffic.push({ method: response.request().method(), path, status: response.status() })
      if (response.status() >= 400
        && !(this.allowInvalidPreflight && path === '/api/v1/config/preflight' && response.status() < 500)
        && !this.allowedFailurePaths.has(path)) {
        this.failures.push(`unexpected ${response.status()} ${path}`)
      }
    })
  }

  async reload(page: Page) {
    for (const [request, meta] of this.requestMeta) {
      if (!meta.finished && meta.method === 'GET' && meta.path.startsWith('/api/')) this.reloadRequests.add(request)
    }
    return page.reload()
  }

  async api(path: string): Promise<any> {
    const response = await fetch(this.origin + path, {
      headers: { Authorization: 'Bearer ' + this.token }, signal: AbortSignal.timeout(5000),
    })
    expect(response.ok, path).toBe(true)
    return response.json()
  }

  async pair(page: Page, info: TestInfo) {
    this.watch(page)
    const document = await page.goto(this.origin + '/settings')
    expect(document?.headers()['content-type']).toContain('text/html')
    // Exercise the actual language control, including a roundtrip for zh.
    await page.getByRole('button', { name: 'EN', exact: true }).click()
    if (info.project.name.startsWith('zh')) await page.getByRole('button', { name: '中', exact: true }).click()
    const english = info.project.name.startsWith('en')
    this.labels = { ...(english ? enConfig : zhConfig), ...(english ? enSettings : zhSettings) } as typeof this.labels
    await expect(page.locator('html')).toHaveAttribute('lang', english ? 'en' : 'zh-Hans')
    const issued = page.waitForResponse(response => new URL(response.url()).pathname === '/api/v0/auth/provision' && response.request().method() === 'POST')
    await page.getByRole('button', { name: this.labels.authGenerate, exact: true }).click()
    this.token = (await (await issued).json()).token
    expect(typeof this.token).toBe('string')
    await page.getByRole('checkbox', { name: this.labels.authAckSaved, exact: true }).check()
    await page.getByRole('button', { name: this.labels.authEnter, exact: true }).click()
    await page.getByRole('button', { name: this.labels.advancedTitle, exact: true }).click()
    const card = this.card(page)
    await expect(card).toBeVisible()
    await card.getByRole('button', { name: this.labels.cfgEnroll, exact: true }).click()
    await expect.poll(async () => (await this.api('/api/v1/config/state')).target.enrolled).toBe(true)
    const cdp = await page.context().browser()!.newBrowserCDPSession()
    const command = await cdp.send('Browser.getBrowserCommandLine')
    const forbidden = command.arguments.filter(value => ['--no-sandbox', '--disable-web-security', '--ignore-certificate-errors'].includes(value))
    expect(forbidden).toEqual([])
    await cdp.detach()
    this.observations.browser = { sandbox_requested: true, forbidden_arguments: forbidden, version: page.context().browser()!.version(), language: english ? 'en' : 'zh', viewport: page.viewportSize() }
    expect([...this.assets].some(path => path.endsWith('.js'))).toBe(true)
    expect([...this.assets].some(path => path.endsWith('.css'))).toBe(true)
  }

  card(page: Page): Locator {
    return page.locator('section.panel').filter({ has: page.getByRole('heading', { name: this.labels.cfgTitle, exact: true }) })
  }

  async snapshot(page: Page, name: string, locator = this.card(page)) {
    await locator.screenshot({ path: join(this.artifact, name + '.png'),
      mask: [page.locator('.token__value'), page.locator('input[type=password]')] })
  }

  async stop(info: TestInfo, page?: Page) {
    if (!this.artifact) return
    if (page && !page.isClosed() && info.status !== info.expectedStatus) {
      await page.screenshot({ path: join(this.artifact, 'failure.png'),
        mask: [page.locator('.token__value'), page.locator('input[type=password]')] }).catch(() => {})
    }
    if (page && !page.isClosed() && this.abortedReads.length > 0) {
      await page.waitForLoadState('networkidle', { timeout: 2000 }).catch(() => {})
    }
    for (const cancelled of this.abortedReads) {
      const successor = this.requests.find(request => request.seq > cancelled.seq && request.seq >= (cancelled.replacement_seq ?? 0) && request.path === cancelled.path
        && request.method === 'GET' && request.finished && request.status !== undefined && request.status >= 200 && request.status < 300)
      if (!successor) this.failures.push(`cancelled API read lacks successful browser successor: ${cancelled.path} #${cancelled.seq}`)
      else {
        cancelled.successor = successor
      }
    }
    this.closing = true
    await page?.context().close().catch(error => this.failures.push('browser cleanup: ' + error.message))
    let forced = false
    if (this.child && this.child.exitCode === null) {
      this.child.kill('SIGINT')
      const until = Date.now() + 3000
      while (this.child.exitCode === null && Date.now() < until) await pause(20)
      if (this.child.exitCode === null) {
        forced = true
        this.child.kill('SIGKILL')
        const killedUntil = Date.now() + 3000
        while (this.child.exitCode === null && Date.now() < killedUntil) await pause(20)
      }
    }
    this.observations.sources_unchanged = !!this.observations.manifest && JSON.stringify(await sourceHashes()) === JSON.stringify(this.observations.manifest.source_inputs)
    this.observations.exit = { code: this.child?.exitCode, signal: this.child?.signalCode, forced }
    this.observations.result = { status: info.status, expected: info.expectedStatus, failures: this.failures, traffic: this.traffic, cancelled_resource_reads: this.abortedReads }
    const serialized = JSON.stringify(this.observations, null, 2)
    if (this.token && serialized.includes(this.token)) throw new Error('Refusing to write fixture bearer to evidence')
    await writeFile(join(this.artifact, 'result.json'), serialized)
    if (this.data && (this.child?.exitCode !== null || this.child?.signalCode)) await rm(this.data, { recursive: true, force: true })
    expect(forced, 'Core must shut down gracefully').toBe(false)
    expect(this.child.exitCode).toBe(0)
    expect(this.observations.sources_unchanged).toBe(true)
    expect(this.failures).toEqual([])
  }
}

export const test = base.extend<{ app: RealCore }>({
  app: [async ({ page }, use, info) => {
    const app = new RealCore()
    try { await app.start(info); await app.pair(page, info); await use(app) }
    finally { await app.stop(info, page) }
  }, { timeout: 25_000 }],
})
export { expect }
