import { defineConfig } from '@playwright/test'
import { tmpdir } from 'node:os'
import { resolve, join } from 'node:path'

// The pinned runner supports this switch; retain only our masked failure image.
process.env.PLAYWRIGHT_NO_COPY_PROMPT = '1'
const evidence = resolve(process.env.HM_BROWSER_EVIDENCE_DIR || join(tmpdir(), 'hm-browser-evidence'))
export default defineConfig({
  testDir: './tests/browser',
  outputDir: join(evidence, 'runner'),
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: !!process.env.CI,
  timeout: 45_000,
  globalTimeout: 180_000,
  expect: { timeout: 5000 },
  reporter: [['line']],
  use: {
    browserName: 'chromium', headless: true,
    launchOptions: { chromiumSandbox: true, args: ['--enable-automation'] },
    actionTimeout: 5000, navigationTimeout: 10_000,
    trace: 'off', screenshot: 'off', video: 'off',
  },
  projects: [
    { name: 'en-desktop', use: { viewport: { width: 1280, height: 800 }, locale: 'en-US' } },
    { name: 'zh-mobile', use: { viewport: { width: 390, height: 844 }, locale: 'zh-CN', isMobile: true } },
  ],
})
