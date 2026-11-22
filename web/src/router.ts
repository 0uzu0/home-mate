/**
 * Six pages plus the setup wizard, lazily loaded.
 *
 * History mode: the core already serves index.html for unknown console paths
 * (`resolve_console_path` in the API), so deep links survive a reload. Each
 * page unmounts on navigation, which stops its poll.
 */
import { createRouter, createWebHistory } from 'vue-router'
import { t, type Key } from './i18n'
import { advancedMode } from './stores/advanced'

const routes = [
  { path: '/', redirect: '/status' },
  {
    path: '/confirmations', name: 'confirmations',
    component: () => import('./views/ConfirmationPage.vue'),
    meta: { titleKey: 'confirmationsTitle' },
  },
  {
    path: '/status',
    name: 'status',
    component: () => import('./views/StatusPage.vue'),
    meta: { titleKey: 'statusTitle' },
  },
  {
    path: '/services',
    name: 'services',
    component: () => import('./views/ServicePage.vue'),
    meta: { titleKey: 'servicesTitle' },
  },
  {
    path: '/tools',
    name: 'tools',
    component: () => import('./views/ToolPage.vue'),
    meta: { titleKey: 'toolsPageTitle' },
  },
  {
    path: '/interaction',
    name: 'interaction',
    component: () => import('./views/InteractionPage.vue'),
    meta: { titleKey: 'interactionTitle' },
  },
  {
    path: '/devices',
    name: 'devices',
    component: () => import('./views/DevicePage.vue'),
    meta: { titleKey: 'devicesTitle' },
  },
  {
    path: '/diagnostics',
    name: 'diagnostics',
    component: () => import('./views/DiagnosticPage.vue'),
    meta: { titleKey: 'diagTitle', advanced: true },
  },
  {
    path: '/releases',
    name: 'releases',
    component: () => import('./views/ReleasePage.vue'),
    meta: { titleKey: 'releaseTitle', advanced: true },
  },
  {
    path: '/settings',
    name: 'settings',
    component: () => import('./views/SettingsPage.vue'),
    meta: { titleKey: 'settingsTitle' },
  },
  {
    path: '/setup',
    name: 'setup',
    component: () => import('./views/SetupPage.vue'),
    meta: { titleKey: 'setupTitle' },
  },
  {
    path: '/:pathMatch(.*)*',
    name: 'not-found',
    component: () => import('./views/NotFoundPage.vue'),
    meta: { titleKey: 'notFoundTitle' },
  },
]

export const router = createRouter({
  history: createWebHistory('/'),
  routes,
  scrollBehavior: () => ({ top: 0 }),
})

router.beforeEach((to) => {
  if (to.meta.advanced && !advancedMode.value) {
    return '/status'
  }
})

const chunkRecoveryKey = 'hm-console-chunk-recovery'

router.onError((error, to) => {
  const message = error instanceof Error ? error.message : String(error)
  if (!/Failed to fetch dynamically imported module|Importing a module script failed|error loading dynamically imported module|Expected a JavaScript(?:-or-Wasm)? module script|Unable to preload CSS|Loading chunk .* failed/i.test(message)) return
  try {
    // Keep the marker across a hard navigation. A second failure at the
    // same URL must remain visible instead of reloading forever.
    if (sessionStorage.getItem(chunkRecoveryKey) === to.fullPath) return
    sessionStorage.setItem(chunkRecoveryKey, to.fullPath)
  } catch {
    return // No durable loop guard is available.
  }
  window.location.assign(to.fullPath)
})

router.afterEach((to, _from, failure) => {
  if (!failure) {
    try { sessionStorage.removeItem(chunkRecoveryKey) } catch { /* Storage may be disabled. */ }
  }
  const key = to.meta.titleKey
  const page = typeof key === 'string' ? t(key as Key) : t('brand')
  document.title = `${page} · ${t('brand')}`
})
