import { test, expect, type RealCore } from './core-fixture'
import type { Page, Locator } from '@playwright/test'
import enServices from '../../src/i18n/en/services'
import zhServices from '../../src/i18n/zh/services'

const PERSONA = 'persona.agent_md.content'
const URL_FIELD = 'services.llm.base_url'
const endpoint = (path: string) => (response: any) => new URL(response.url()).pathname === path && response.request().method() === 'POST'

async function row(card: Locator, page: Page, path: string): Promise<Locator> {
  const field = card.locator('.field-row').filter({ has: page.getByText(path, { exact: true }) })
  await field.getByRole('checkbox').first().check()
  return field
}

async function published(app: RealCore, page: Page, id: string, desired: unknown) {
  const card = app.card(page)
  let readback: any
  await expect.poll(async () => {
    const response = page.waitForResponse(value => new URL(value.url()).pathname === `/api/v1/config/applications/${id}/readback`)
    await card.getByRole('button', { name: app.labels.cfgReadbackRefresh, exact: true }).click()
    readback = await (await response).json()
    return readback.publications?.find((value: any) => value.block === 'persona')?.state
  }, { timeout: 5000, intervals: [50, 100, 250] }).toBe('applied')
  expect(readback.initial_receipt.application_id).toBe(id)
  const field = readback.fields.find((value: any) => value.path === PERSONA)
  expect(field.state).toBe('core_readback_verified')
  expect(field.value).toEqual(desired)
  await expect(card.getByText(app.labels.cfgPublicationApplied, { exact: false })).toBeVisible()
  await expect(card.getByText(app.labels.cfgReaderVerified, { exact: false })).toBeVisible()
  expect((await app.api(`/api/v1/config/applications/${id}/readback`)).observed_revision).toBe(readback.observed_revision)
  return readback
}

async function submit(app: RealCore, page: Page) {
  const response = page.waitForResponse(endpoint('/api/v1/config/applications'))
  await app.card(page).getByRole('button', { name: app.labels.cfgCommit, exact: true }).click()
  const receipt = await (await response).json()
  expect(receipt.state).toBe('saved_pending')
  expect(receipt.application_id).toBeTruthy()
  return receipt
}

async function noPageOverflow(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
}

async function history(app: RealCore, page: Page, id: string) {
  const item = app.card(page).locator('li').filter({ has: page.getByText(id, { exact: true }) })
  await item.getByRole('button', { name: app.labels.cfgDetails, exact: true }).click()
}

test('persona reviewed plan publishes, survives reload and rolls back through real UI', async ({ page, app }) => {
  const card = app.card(page)
  const baseline = await app.api('/api/v1/config/state')
  const native = await app.api('/api/v0/agent-md')
  const desired = `Synthetic browser persona ${test.info().project.name}.`
  const field = await row(card, page, PERSONA)
  await field.getByLabel(app.labels.cfgDesired, { exact: true }).fill(desired)
  await card.getByLabel(app.labels.cfgReason, { exact: true }).fill('synthetic browser forward')
  const preflight = page.waitForResponse(endpoint('/api/v1/config/preflight'))
  await card.getByRole('button', { name: app.labels.cfgPreview, exact: true }).click()
  const plan = await (await preflight).json()
  expect(plan.plan_id).toBeTruthy()
  expect(plan.plan_hash).toMatch(/^[a-f0-9]{64}$/)
  expect(plan.expected_revision).toBe(baseline.config_revision)
  expect(plan.blocked).toEqual([])
  expect(plan.diff).toHaveLength(1)
  expect(plan.diff[0]).toMatchObject({ path: PERSONA, before: baseline.stored[PERSONA], after: desired })
  await expect(card.getByText(plan.plan_hash, { exact: true })).toBeVisible()
  const afterPreview = await app.api('/api/v1/config/state')
  expect(afterPreview.config_revision).toBe(baseline.config_revision)
  expect(afterPreview.applications).toEqual(baseline.applications)
  expect((await app.api('/api/v0/agent-md')).content).toBe(native.content)
  await noPageOverflow(page)
  await app.snapshot(page, 'reviewed-diff', card.locator('.cfg-section').filter({ has: page.getByRole('heading', { name: app.labels.cfgReview, exact: true }) }))
  const receipt = await submit(app, page)
  const applied = await published(app, page, receipt.application_id, desired)
  expect((await app.api('/api/v0/agent-md')).content).toBe(desired)
  await app.snapshot(page, 'published-result', card.locator('.cfg-section').filter({ has: page.getByRole('heading', { name: app.labels.cfgResult, exact: true }) }))
  await app.reload(page)
  await expect(card).toBeVisible()
  const persisted = await app.api('/api/v1/config/state')
  expect(persisted.stored[PERSONA]).toBe(desired)
  await history(app, page, receipt.application_id)
  await expect(card.getByRole('button', { name: app.labels.cfgRollbackPreview, exact: true })).toBeEnabled()
  await published(app, page, receipt.application_id, desired)
  const rollbackResponse = page.waitForResponse(endpoint(`/api/v1/config/applications/${receipt.application_id}/rollback-preflight`))
  await card.getByRole('button', { name: app.labels.cfgRollbackPreview, exact: true }).click()
  const rollbackPlan = await (await rollbackResponse).json()
  expect(rollbackPlan.diff[0]).toMatchObject({ path: PERSONA, before: desired, after: baseline.stored[PERSONA] })
  expect(rollbackPlan.expected_revision).toBe(receipt.config_revision)
  const rollback = await submit(app, page)
  expect(rollback.application_id).not.toBe(receipt.application_id)
  expect(rollback.config_revision).toBeGreaterThan(receipt.config_revision)
  const restored = await published(app, page, rollback.application_id, baseline.stored[PERSONA])
  expect((await app.api('/api/v0/agent-md')).content).toBe(native.content)
  await app.reload(page)
  const final = await app.api('/api/v1/config/state')
  expect(final.stored).toEqual(baseline.stored)
  expect(final.data_epoch).toBe(baseline.data_epoch)
  expect(final.applications.map((value: any) => value.application_id).sort()).toEqual([...baseline.applications.map((value: any) => value.application_id), receipt.application_id, rollback.application_id].sort())
  await history(app, page, rollback.application_id)
  await published(app, page, rollback.application_id, baseline.stored[PERSONA])
  expect(app.traffic.filter(value => value.method === 'POST' && value.path === '/api/v1/config/applications')).toHaveLength(2)
  await noPageOverflow(page)
  await app.snapshot(page, 'rollback-result', card.locator('.cfg-section').filter({ has: page.getByRole('heading', { name: app.labels.cfgResult, exact: true }) }))
  app.observations.persona = { baseline_revision: baseline.config_revision, baseline_value: baseline.stored[PERSONA], plan, receipt, applied, rollbackPlan, rollback, restored, final_revision: final.config_revision }
})

test('invalid URL preflight is visibly rejected and never writes an application', async ({ page, app }) => {
  const card = app.card(page)
  const baseline = await app.api('/api/v1/config/state')
  const native = await app.api('/api/v0/config/services')
  const field = await row(card, page, URL_FIELD)
  await field.getByLabel(app.labels.cfgDesired, { exact: true }).fill('not-a-url')
  await card.getByLabel(app.labels.cfgReason, { exact: true }).fill('synthetic browser invalid URL')
  app.allowInvalidPreflight = true
  const rejected = page.waitForResponse(endpoint('/api/v1/config/preflight'))
  await card.getByRole('button', { name: app.labels.cfgPreview, exact: true }).click()
  const response = await rejected
  expect(response.status()).toBeGreaterThanOrEqual(400)
  expect(response.status()).toBeLessThan(500)
  const error = await response.json()
  expect(JSON.stringify(error)).toContain('INVALID_SERVICE_URL')
  await expect(card.getByText(/INVALID_SERVICE_URL/)).toBeVisible()
  await expect(field.getByLabel(app.labels.cfgDesired, { exact: true })).toHaveValue('not-a-url')
  await expect(card.getByLabel(app.labels.cfgReason, { exact: true })).toHaveValue('synthetic browser invalid URL')
  await expect(card.getByRole('button', { name: app.labels.cfgCommit, exact: true })).toHaveCount(0)
  const after = await app.api('/api/v1/config/state')
  expect(after.config_revision).toBe(baseline.config_revision)
  expect(after.applications).toEqual(baseline.applications)
  expect(after.stored).toEqual(baseline.stored)
  expect(await app.api('/api/v0/config/services')).toEqual(native)
  expect(app.traffic.filter(value => value.method === 'POST' && value.path === '/api/v1/config/applications')).toEqual([])
  await noPageOverflow(page)
  await app.snapshot(page, 'preflight-error')
  await app.reload(page)
  expect((await app.api('/api/v1/config/state')).stored).toEqual(baseline.stored)
  await expect(card.getByText('not-a-url', { exact: true })).toHaveCount(0)
  expect(app.traffic.filter(value => value.method === 'POST' && value.path === '/api/v1/config/preflight')).toHaveLength(1)
  app.observations.rejected = { baseline_revision: baseline.config_revision, status: response.status(), error, final_revision: after.config_revision, application_count: after.applications.length }
})

test('Music connections use reviewed replacement and explicit disable in the real console', async ({ page, app }, info) => {
  const labels = info.project.name === 'zh-mobile' ? zhServices : enServices
  const card = app.card(page)
  const baseline = await app.api('/api/v0/config/services/music')
  const library = await row(card, page, 'music.library')
  const online = await row(card, page, 'music.online')
  const connection = { endpoint: 'http://127.0.0.1:14533', user: 'browser-fixture' }
  await library.getByLabel(labels.musicEndpoint, { exact: true }).fill(connection.endpoint)
  await library.getByLabel(labels.musicUser, { exact: true }).fill(connection.user)
  await online.getByLabel(app.labels.cfgAction, { exact: true }).selectOption({ label: app.labels.cfgDisableMusicConnection })
  await card.getByLabel(app.labels.cfgReason, { exact: true }).fill('synthetic reviewed Music connections')
  const preview = page.waitForResponse(endpoint('/api/v1/config/preflight'))
  await card.getByRole('button', { name: app.labels.cfgPreview, exact: true }).click()
  const plan = await (await preview).json()
  expect(plan.diff).toEqual(expect.arrayContaining([
    expect.objectContaining({ path: 'music.library', action: 'replace', after: connection }),
    expect.objectContaining({ path: 'music.online', action: 'set', layer_after: null, layer_after_null: true }),
  ]))
  expect(await app.api('/api/v0/config/services/music')).toEqual(baseline)
  const receipt = await submit(app, page)
  await expect.poll(async () => (await app.api('/api/v1/config/applications/' + receipt.application_id + '/readback'))
    .publications.find((value: any) => value.block === 'music')?.state).toBe('applied')
  await card.getByRole('button', { name: app.labels.cfgReadbackRefresh, exact: true }).click()
  await expect(card.getByText(app.labels.cfgMusicPublication + ': ' + app.labels.cfgPublicationApplied)).toBeVisible()
  expect((await app.api('/api/v0/config/services/music')).library).toEqual({ ...connection, secret_set: false })
  await noPageOverflow(page)
})

test('OpenAPI catalogue replacement and header readback use the real console', async ({ page, app }) => {
  const card = app.card(page)
  const baseline = await app.api('/api/v0/config/services/openapi')
  const spec = await row(card, page, 'openapi.spec_url')
  const header = await row(card, page, 'openapi.auth_header')
  const catalog = await row(card, page, 'openapi.operations')
  await spec.getByLabel(app.labels.cfgDesired, { exact: true }).fill('https://openapi.example.invalid/schema.json')
  await header.getByLabel(app.labels.cfgDesired, { exact: true }).fill('X-Fixture-Auth')
  await catalog.getByLabel(app.labels.cfgOpenApiOperations, { exact: true }).fill(JSON.stringify([
    { id: 'status', method: 'get', path: '/status' },
  ]))
  await card.getByLabel(app.labels.cfgReason, { exact: true }).fill('synthetic reviewed OpenAPI catalogue')
  const preview = page.waitForResponse(endpoint('/api/v1/config/preflight'))
  await card.getByRole('button', { name: app.labels.cfgPreview, exact: true }).click()
  const plan = await (await preview).json()
  expect(plan.diff).toEqual(expect.arrayContaining([
    expect.objectContaining({ path: 'openapi.operations', action: 'replace', after: [
      expect.objectContaining({ id: 'status', path: '/status' }),
    ] }),
    expect.objectContaining({ path: 'openapi.auth_header', action: 'set', after: 'X-Fixture-Auth' }),
  ]))
  expect(await app.api('/api/v0/config/services/openapi')).toEqual(baseline)

  const receipt = await submit(app, page)
  await expect.poll(async () => (await app.api('/api/v1/config/applications/' + receipt.application_id + '/readback'))
    .publications.find((value: any) => value.block === 'openapi')?.state).toBe('applied')
  await card.getByRole('button', { name: app.labels.cfgReadbackRefresh, exact: true }).click()
  await expect(card.getByText(app.labels.cfgOpenApiPublication + ': ' + app.labels.cfgPublicationApplied)).toBeVisible()
  const active = await app.api('/api/v0/config/services/openapi')
  expect(active.auth_header).toBe('X-Fixture-Auth')
  expect(active.operations.map((op: any) => op.id)).toEqual(['status'])
  expect(active.revision).not.toEqual(baseline.revision)
  await noPageOverflow(page)
})

test('Voice policy review publishes runtime values with an explicit unlimited choice', async ({ page, app }) => {
  const card = app.card(page)
  const baseline = await app.api('/api/v0/mode')
  const name = await row(card, page, 'mode.assistant_name')
  const credits = await row(card, page, 'mode.followup_credits')
  const retreat = await row(card, page, 'mode.retreat_words')
  const budget = await row(card, page, 'mode.spoken.max_chars')
  await name.getByLabel(app.labels.cfgDesired, { exact: true }).fill('Reviewed voice fixture')
  await credits.getByLabel(app.labels.cfgAction, { exact: true }).selectOption({ label: app.labels.cfgFollowupUnlimited })
  await retreat.getByLabel(app.labels.cfgVoiceWordList, { exact: true }).fill(JSON.stringify(['再会试验']))
  await budget.getByLabel(app.labels.cfgDesired, { exact: true }).fill('96')
  await card.getByLabel(app.labels.cfgReason, { exact: true }).fill('synthetic reviewed voice policy')
  const preview = page.waitForResponse(endpoint('/api/v1/config/preflight'))
  await card.getByRole('button', { name: app.labels.cfgPreview, exact: true }).click()
  const plan = await (await preview).json()
  expect(plan.diff).toEqual(expect.arrayContaining([
    expect.objectContaining({ path: 'mode.assistant_name', after: 'Reviewed voice fixture' }),
    expect.objectContaining({ path: 'mode.followup_credits', action: 'set', layer_after_null: true }),
    expect.objectContaining({ path: 'mode.retreat_words', action: 'replace', after: ['再会试验'] }),
    expect.objectContaining({ path: 'mode.spoken.max_chars', action: 'set', after: 96 }),
  ]))
  expect((await app.api('/api/v0/mode')).assistant_name).toBe(baseline.assistant_name)
  const receipt = await submit(app, page)
  await expect.poll(async () => (await app.api('/api/v1/config/applications/' + receipt.application_id + '/readback'))
    .publications.find((value: any) => value.block === 'mode')?.state).toBe('applied')
  await card.getByRole('button', { name: app.labels.cfgReadbackRefresh, exact: true }).click()
  await expect(card.getByText(app.labels.cfgVoicePublication + ': ' + app.labels.cfgPublicationApplied)).toBeVisible()
  const active = await app.api('/api/v0/mode')
  expect(active.assistant_name).toBe('Reviewed voice fixture')
  expect(active.followup_credits).toBeNull()
  expect(active.retreat_words).toEqual(['再会试验'])
  expect(active.spoken.max_chars).toBe(96)
  expect(active.tail_blind_ms).toEqual(baseline.tail_blind_ms)
  await noPageOverflow(page)
})

test('Open sockets replace a node registry and roll back through the console', async ({ page, app }) => {
  const card = app.card(page)
  const route = '/api/v0/config/sockets'
  const baseline = await app.api(route)
  const contracts = await row(card, page, 'open_sockets')
  await contracts.getByLabel(app.labels.cfgOpenSocketsContracts, { exact: true }).fill(JSON.stringify([{
    name: 'fixture.room_status', description: 'Read room status', effect: 'Query',
    endpoint_url: 'https://socket.example.invalid/status',
  }]))
  await card.getByLabel(app.labels.cfgReason, { exact: true }).fill('Review a synthetic tool registry')

  const preview = page.waitForResponse(endpoint('/api/v1/config/preflight'))
  await card.getByRole('button', { name: app.labels.cfgPreview, exact: true }).click()
  const plan = await (await preview).json()
  expect(plan.diff).toEqual(expect.arrayContaining([
    expect.objectContaining({ path: 'open_sockets', action: 'replace',
      after: [expect.objectContaining({ name: 'fixture.room_status' })] }),
  ]))
  expect(await app.api(route)).toEqual(baseline)

  const observe = async (id: string) => {
    const path = '/api/v1/config/applications/' + id + '/readback'
    await expect.poll(async () => (await app.api(path))
      .publications.find((value: any) => value.block === 'open_sockets')?.state).toBe('applied')
    await card.getByRole('button', { name: app.labels.cfgReadbackRefresh, exact: true }).click()
    await expect(card.getByText(app.labels.cfgOpenSocketsPublication + ': ' + app.labels.cfgPublicationApplied)).toBeVisible()
    const evidence = await app.api(path)
    expect(evidence.fields.find((field: any) => field.path === 'open_sockets')?.state).toBe('core_readback_verified')
  }
  const receipt = await submit(app, page)
  await observe(receipt.application_id)
  expect((await app.api(route)).sockets.map((entry: any) => entry.name)).toEqual(['fixture.room_status'])
  await noPageOverflow(page)
  await app.reload(page)
  await history(app, page, receipt.application_id)
  const rollback = page.waitForResponse(endpoint('/api/v1/config/applications/' + receipt.application_id + '/rollback-preflight'))
  await card.getByRole('button', { name: app.labels.cfgRollbackPreview, exact: true }).click()
  expect((await (await rollback).json()).diff[0]).toMatchObject({ path: 'open_sockets', after: baseline.sockets })
  await observe((await submit(app, page)).application_id)
  expect(await app.api(route)).toEqual(baseline)
})
