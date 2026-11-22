import { test, expect, type RealCore } from './core-fixture'
import type { Page, Locator } from '@playwright/test'
import enConfig from '../../src/i18n/en/config'
import zhConfig from '../../src/i18n/zh/config'
import enSettings from '../../src/i18n/en/settings'
import zhSettings from '../../src/i18n/zh/settings'
import enShell from '../../src/i18n/en/shell'
import zhShell from '../../src/i18n/zh/shell'
import enStatus from '../../src/i18n/en/status'
import zhStatus from '../../src/i18n/zh/status'

function dict(projectName: string) {
  const en = projectName.startsWith('en')
  return {
    ...(en ? enConfig : zhConfig),
    ...(en ? enSettings : zhSettings),
    ...(en ? enShell : zhShell),
    ...(en ? enStatus : zhStatus),
  }
}

function panelByTitle(page: Page, title: string): Locator {
  return page.locator('section.panel').filter({ has: page.getByRole('heading', { name: title, exact: true }) })
}

test('UI-01 AgentPersonaCard blocks save before load, recovers via retry, and reads back save + rollback', async ({ page, app }, info) => {
  const l = dict(info.project.name)
  const baseline = await app.api('/api/v0/agent-md')

  // Inject a transient 503 on the initial Agent.md load (including useResource's 1 auto-retry).
  app.allowedFailurePaths.add('/api/v0/agent-md')
  let failAgentMd = true
  await page.route('**/api/v0/agent-md', async (route) => {
    if (failAgentMd && route.request().method() === 'GET') {
      await route.fulfill({
        status: 503,
        contentType: 'application/json',
        body: JSON.stringify({ code: 'unavailable', detail: 'injected initial load fault' }),
      })
      return
    }
    await route.continue()
  })

  await page.getByRole('tab', { name: l.settingsTabAssistant, exact: true }).click()
  const card = panelByTitle(page, l.agentPersonaTitle)
  await expect(card).toBeVisible()

  // Before initial load succeeds: textarea and Save must stay disabled, ErrorNotice + Retry visible.
  const textarea = card.getByLabel(l.agentMdHeading, { exact: true })
  const saveBtn = card.getByRole('button', { name: l.agentMdSave, exact: true })
  await expect(card.locator('.error')).toBeVisible()
  await expect(textarea).toBeDisabled()
  await expect(saveBtn).toBeDisabled()

  // Click Retry -> real GET /api/v0/agent-md succeeds -> data/version displayed and controls unlock.
  failAgentMd = false
  app.allowedFailurePaths.delete('/api/v0/agent-md')
  await card.locator('.error__retry').first().click()
  await expect(textarea).toBeEnabled()
  await expect(saveBtn).toBeEnabled()
  await expect(card.locator('.ap__ver')).toHaveText(`v${baseline.version}`)
  await expect(textarea).toHaveValue(baseline.content)

  // Save two real revisions (v1 -> v2) and verify readback + version bump against real hm-core.
  const firstContent = `Real browser UI-01 persona v1 (${info.project.name})`
  await textarea.fill(firstContent)
  await card.getByLabel(l.agentMdNoteLabel, { exact: true }).fill('ui-01 save v1')
  const saved1 = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/agent-md' && r.request().method() === 'PUT')
  await saveBtn.click()
  expect((await saved1).status()).toBe(200)
  const v1 = baseline.version + 1
  await expect(card.locator('.ap__ver')).toHaveText(`v${v1}`)
  expect((await app.api('/api/v0/agent-md')).content).toBe(firstContent)

  const secondContent = `Real browser UI-01 persona v2 (${info.project.name})`
  await textarea.fill(secondContent)
  await card.getByLabel(l.agentMdNoteLabel, { exact: true }).fill('ui-01 save v2')
  const saved2 = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/agent-md' && r.request().method() === 'PUT')
  await saveBtn.click()
  expect((await saved2).status()).toBe(200)
  await expect(card.locator('.ap__ver')).toHaveText(`v${v1 + 1}`)
  expect((await app.api('/api/v0/agent-md')).content).toBe(secondContent)

  // Rollback via revision history to v1 and verify real readback restores v1 content.
  await card.locator('details.ap__history summary').click()
  const rollbackResp = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/agent-md/rollback' && r.request().method() === 'POST')
  await card.locator('.ap__rev').filter({ hasText: `v${v1}` }).getByRole('button', { name: l.agentMdRollback, exact: true }).click()
  expect((await rollbackResp).status()).toBe(200)
  await expect(textarea).toHaveValue(firstContent)
  expect((await app.api('/api/v0/agent-md')).content).toBe(firstContent)

  // Verify USER.md section is loaded & writable against real hm-core.
  await expect(card.getByRole('heading', { name: l.userMdHeading, exact: true })).toBeVisible()
  await card.getByLabel(l.userMdKey, { exact: true }).fill('ui01_pref')
  await card.getByLabel(l.userMdContent, { exact: true }).fill(`pref-${info.project.name}`)
  const userPut = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/user-md' && r.request().method() === 'PUT')
  await card.getByRole('button', { name: l.userMdAdd, exact: true }).click()
  expect((await userPut).status()).toBe(200)
  await expect(card.getByText(`pref-${info.project.name}`, { exact: true })).toBeVisible()
})

test('UI-02 WorkingMemoryCard preserves dirty shared-memory draft across polls, supports reload discard and real save', async ({ page, app }, info) => {
  const l = dict(info.project.name)
  await page.getByRole('tab', { name: l.settingsTabAssistant, exact: true }).click()
  const card = panelByTitle(page, l.workingMemoryTitle)
  await expect(card).toBeVisible()

  const shared = card.locator('.wm__shared')
  const nasInput = shared.getByLabel(l.sharedMemoryNasUrl, { exact: true })
  const saveBtn = shared.getByRole('button', { name: l.sharedMemorySaveConfig, exact: true })
  const reloadBtn = shared.getByRole('button', { name: l.configReloadDraft, exact: true })
  await expect(saveBtn).toBeEnabled()

  const baseline = await app.api('/api/v0/shared-memory/status')

  // Edit into a dirty draft; wait for a real background poll of /api/v0/shared-memory/status.
  const draftUrl = 'http://127.0.0.1:18999'
  await nasInput.fill(draftUrl)
  await expect(shared.getByText(l.configDraftKept, { exact: true })).toBeVisible()

  // Wait for the real 15s background poll of GET /api/v0/shared-memory/status and verify draft survives.
  const pollSeen = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/shared-memory/status' && r.request().method() === 'GET', { timeout: 20_000 })
  await pollSeen
  await expect(nasInput).toHaveValue(draftUrl)
  await expect(shared.getByText(l.configDraftKept, { exact: true })).toBeVisible()

  // Explicit reload discards the dirty draft and restores real server value.
  await reloadBtn.click()
  await expect(nasInput).toHaveValue(baseline.nas_base_url)
  await expect(shared.getByText(l.configDraftKept, { exact: true })).toBeHidden()

  // Real save persists to hm-core and clears dirty indicator via readback.
  const savedUrl = 'http://127.0.0.1:18084'
  await nasInput.fill(savedUrl)
  const putResp = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/shared-memory/config' && r.request().method() === 'PUT')
  await saveBtn.click()
  expect((await putResp).status()).toBe(200)
  await expect(shared.getByText(l.configDraftKept, { exact: true })).toBeHidden()
  await expect(nasInput).toHaveValue(savedUrl)
  expect((await app.api('/api/v0/shared-memory/status')).nas_base_url).toBe(savedUrl)
})

test('UI-03 FleetCard initializes clean, dirties only on real edit, preserves draft on refresh, and saves', async ({ page, app }, info) => {
  const l = dict(info.project.name)
  // app.pair() already enabled Advanced Mode and switched to the Advanced & Fleet tab.
  const card = panelByTitle(page, l.fleetTitle)
  await expect(card).toBeVisible()

  const baseline = await app.api('/api/v0/fleet')
  const selfInput = card.getByLabel(l.fleetSelfId, { exact: true })
  const saveBtn = card.locator('.fleet__row').getByRole('button', { name: l.save, exact: true })
  const reloadDraftBtn = card.locator('.fleet__row').getByRole('button', { name: l.configReloadDraft, exact: true })
  const refreshBtn = card.getByRole('button', { name: l.refresh, exact: true })

  // Initial load: populated from real hm-core, NOT dirty (Save disabled, Reload draft hidden).
  await expect(selfInput).toHaveValue(baseline.self_id)
  await expect(saveBtn).toBeDisabled()
  await expect(reloadDraftBtn).toBeHidden()

  // Real edit marks dirty; Refresh preserves the edited draft instead of clobbering it.
  const editedId = `ui03-${info.project.name}`
  await selfInput.fill(editedId)
  await expect(saveBtn).toBeEnabled()
  await expect(reloadDraftBtn).toBeVisible()

  const refreshResp = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/fleet' && r.request().method() === 'GET')
  await refreshBtn.click()
  expect((await refreshResp).status()).toBe(200)
  await expect(selfInput).toHaveValue(editedId)
  await expect(saveBtn).toBeEnabled()

  // Save persists to real hm-core and clears dirty state.
  const saveResp = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/fleet' && r.request().method() === 'PUT')
  await saveBtn.click()
  expect((await saveResp).status()).toBe(200)
  await expect(saveBtn).toBeDisabled()
  expect((await app.api('/api/v0/fleet')).self_id).toBe(editedId)
})

test('UI-04 DeviceFreshnessNotice keeps prior data with stale timestamp on failure without advancing success time', async ({ page, app }, info) => {
  const l = dict(info.project.name)

  // Open Status page and wait for initial real /api/v0/mode load to succeed.
  const firstMode = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/mode' && r.status() === 200)
  await page.goto(app.origin + '/status')
  await firstMode
  const noticeSection = page.locator(`section[aria-label="${l.modeSnapshotLabel}"]`)
  await expect(noticeSection).toBeHidden()

  // Inject a 503 on subsequent /api/v0/mode polls (while gateway/meta remain healthy).
  app.allowedFailurePaths.add('/api/v0/mode')
  let failMode = true
  await page.route('**/api/v0/mode', async (route) => {
    if (failMode && route.request().method() === 'GET') {
      await route.fulfill({
        status: 503,
        contentType: 'application/json',
        body: JSON.stringify({ code: 'unavailable', detail: 'injected mode poll failure' }),
      })
      return
    }
    await route.continue()
  })

  // Wait for the next real poll to hit /api/v0/mode and fail -> stale notice appears with last-success time.
  await expect(noticeSection).toBeVisible({ timeout: 15_000 })
  const staleNotice = noticeSection.locator('.error--stale')
  await expect(staleNotice).toBeVisible()
  // Retained prior StatusPage data stays visible and healthy sibling resources stay clean (per-resource isolation).
  await expect(panelByTitle(page, l.readinessTitle)).toBeVisible()
  await expect(page.getByText(l.statusNoDevices, { exact: true })).toBeVisible()
  await expect(page.locator(`section[aria-label="${l.gatewaySnapshotLabel}"]`)).toBeHidden()
  await expect(page.locator(`section[aria-label="${l.metadataSnapshotLabel}"]`)).toBeHidden()

  const staleText1 = (await staleNotice.locator('.error__stale').textContent())?.trim() ?? ''
  expect(staleText1.length).toBeGreaterThan(0)

  // Cross a real 1-second wall-clock boundary before a second failed attempt so 1s-resolution
  // formatTime() would deterministically change if a regression advanced lastSuccessAt on failure.
  const secBefore = Math.floor(Date.now() / 1000)
  await expect.poll(() => Math.floor(Date.now() / 1000), { intervals: [100], timeout: 2500 }).toBeGreaterThan(secBefore)
  const secondFail = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/mode' && r.status() === 503, { timeout: 15_000 })
  await staleNotice.locator('.error__retry').click()
  await secondFail
  const staleText2 = (await staleNotice.locator('.error__stale').textContent())?.trim() ?? ''
  expect(staleText2).toBe(staleText1)

  // Restore /api/v0/mode and dispatch Retry synchronously so the 500ms poll cannot race unmount/in-flight 503s.
  failMode = false
  const recovered = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v0/mode' && r.status() === 200)
  await staleNotice.locator('.error__retry').evaluate((el: HTMLButtonElement) => el.click())
  await recovered
  await expect(noticeSection).toBeHidden()
  await page.unroute('**/api/v0/mode')
  app.allowedFailurePaths.delete('/api/v0/mode')
})
