<script setup lang="ts">
/**
 * AppShell: brand, seven-page navigation, the one device selector, connection
 * state, language and theme.
 *
 * The device selection is app-wide: pages read the shared store, the selector
 * writes it, and the URL carries it as `?device=` so a link can point at one
 * device (the same idea as RUI's `/devices/:id`, without a nested route).
 */
import { computed, onMounted, ref, watch } from 'vue'
import { RouterLink, RouterView, useRoute, useRouter } from 'vue-router'
import { getAuthStatus, getStatus } from './api'
import { authToken, pairingRequired } from './auth'
import { locale, setLocale, t, type Key } from './i18n'
import { cycleTheme, themePref } from './theme'
import { advancedMode, toggleAdvancedMode } from './stores/advanced'
import { useResource } from './hooks/useResource'
import { usePoll } from './hooks/usePoll'
import {
  deviceName,
  deviceRoom,
  devices,
  refresh as refreshDevices,
  refreshSilent as refreshDevicesSilent,
  selectDevice,
  selected,
  selectedId,
  setStatusSnapshot,
} from './stores/devices'
import type { IconName } from './components/icons'
import Toast from './components/Toast.vue'
import Pill from './components/Pill.vue'
import AppIcon from './components/AppIcon.vue'
import BrandLogo from './components/BrandLogo.vue'
import AuthGate from './components/AuthGate.vue'
import ConfirmationNotice from './components/confirmations/ConfirmationNotice.vue'

const route = useRoute()
const router = useRouter()

// ── M9 household access ──────────────────────────────────────────────
// The shell only mounts once a token is in hand, so no page can poll the
// management API unauthenticated. `GET /api/v0/auth` is open precisely so a
// fresh device can say "set up" instead of "401".
type AuthView = 'checking' | 'setup' | 'pair' | 'corrupt' | 'ready'
const authView = ref<AuthView>('checking')

async function bootAuth(): Promise<void> {
  pairingRequired.value = false
  authView.value = 'checking'
  try {
    const state = await getAuthStatus({ timeoutMs: 4000 })
    if (state.corrupt) authView.value = 'corrupt'
    else if (!state.configured) authView.value = 'setup'
    else if (!authToken.value) authView.value = 'pair'
    else authView.value = 'ready'
  } catch {
    // Core unreachable: keep a usable shell only when this browser already
    // holds a credential (the status pill explains the outage); otherwise the
    // gate is the honest place, with its own retry.
    authView.value = authToken.value ? 'ready' : 'pair'
  }
}

onMounted(bootAuth)
// Any 401 anywhere in the app (rotation in another tab, revoked token, factory
// reset) flips this; ignore during first-boot setup so the one-time token is
// never unmounted mid-copy, and re-probe via bootAuth() on ready -> unauthorized
// so factory-reset devices land on 'setup' instead of looping on 'pair'.
watch(pairingRequired, (needs) => {
  if (!needs || authView.value === 'setup') return
  if (authView.value === 'ready') {
    void bootAuth()
  }
})

const status = useResource(
  (signal) => getStatus({ signal, device: selectedId.value || undefined }),
  { timeoutMs: 5000, retries: 1 },
)
watch(
  () => status.data.value,
  (snap) => {
    setStatusSnapshot(snap)
  },
  { immediate: true },
)
// Pause background status polling while the auth gate is visible so an
// unauthenticated /status request cannot 401 right after first-boot provision
// and destroy the one-time token screen mid-copy.
usePoll(() => (authView.value === 'ready' ? status.load() : Promise.resolve()), 10000)
// The moment a token is accepted (or bootAuth finishes ready), load status and
// devices immediately instead of waiting for the next poll interval.
watch(authView, (view, previous) => {
  if (view === 'ready' && previous !== 'ready') {
    void status.load()
    void refreshDevices()
  }
})
usePoll(() => (authView.value === 'ready' ? refreshDevicesSilent() : Promise.resolve()), 500)

const allLinks: Array<{ path: string; key: Key; icon: IconName; advanced?: boolean }> = [
  { path: '/status', key: 'navStatus', icon: 'status' },
  { path: '/confirmations', key: 'navConfirmations', icon: 'tools' },
  { path: '/services', key: 'navServices', icon: 'services' },
  { path: '/tools', key: 'navTools', icon: 'tools' },
  { path: '/interaction', key: 'navInteraction', icon: 'interaction' },
  { path: '/devices', key: 'navDevices', icon: 'devices' },
  { path: '/releases', key: 'navRelease', icon: 'package', advanced: true },
  { path: '/diagnostics', key: 'navDiagnostics', icon: 'diagnostics', advanced: true },
  { path: '/settings', key: 'navSettings', icon: 'settings' },
]

const links = computed(() => allLinks.filter((link) => !link.advanced || advancedMode.value))

watch(advancedMode, (on) => {
  if (!on && route.meta.advanced) {
    void router.replace('/status')
  }
})

// Four states, because "connected" was a lie in three of them (2026-11-14
// audit): the first frame has no data yet (a red "not connected" before the
// first poll ever returned), and a single failed poll turned a working shell
// red while the last good data was still on screen.
//   connecting — no answer yet, nothing failed
//   ok         — data, latest refresh succeeded
//   stale      — data from an earlier poll, latest refresh failed (keep it,
//                name the failure; a stale number presented as current is the
//                console lying)
//   offline    — no data at all and the request failed
type ConnState = 'connecting' | 'ok' | 'stale' | 'offline'
const connState = computed<ConnState>(() => {
  if (status.data.value === null) {
    return status.error.value === null ? 'connecting' : 'offline'
  }
  return status.error.value === null ? 'ok' : 'stale'
})
const connReason = computed(() => {
  const err = status.error.value
  if (err === null) return ''
  const e = err as { message?: string; code?: string }
  return (e.message || e.code || String(err)).slice(0, 60)
})
const connTone = computed<'neutral' | 'ok' | 'warn' | 'danger'>(() => {
  switch (connState.value) {
    case 'ok':
      return 'ok'
    case 'stale':
      return 'warn'
    case 'offline':
      return 'danger'
    default:
      return 'neutral'
  }
})
const connIcon = computed<IconName>(() => {
  switch (connState.value) {
    case 'ok':
      return 'check'
    case 'stale':
      return 'alert'
    case 'offline':
      return 'fail'
    default:
      return 'loading'
  }
})
const connLabel = computed(() => {
  switch (connState.value) {
    case 'ok':
      return t('appConnected')
    case 'stale':
      return t('appStale', { reason: connReason.value })
    case 'offline':
      return t('appDisconnected')
    default:
      return t('appConnecting')
  }
})
const version = computed(() => status.data.value?.version ?? '—')
const pageTitle = computed(() => {
  const key = route.meta.titleKey
  return typeof key === 'string' ? t(key as Key) : t('brand')
})

const themeIcon = computed<IconName>(() => {
  switch (themePref.value) {
    case 'light':
      return 'themeLight'
    case 'dark':
      return 'themeDark'
    default:
      return 'themeAuto'
  }
})

function themeShort(): string {
  switch (themePref.value) {
    case 'light':
      return t('themeLight')
    case 'dark':
      return t('themeDark')
    default:
      return t('themeAuto')
  }
}

function navTo(path: string) {
  return selectedId.value ? { path, query: { device: selectedId.value } } : { path }
}

function chooseDevice(event: Event): void {
  const id = (event.target as HTMLSelectElement).value
  if (id) selectDevice(id)
}

function switchLanguage(): void {
  setLocale(locale.value === 'zh' ? 'en' : 'zh')
}

// The router sets the title on navigation; a language change must retitle the
// page already open without a reload.
watch(
  pageTitle,
  (title) => {
    document.title = `${title} · ${t('brand')}`
  },
  { immediate: true },
)

// ── Selection <-> URL ────────────────────────────────────────────────
// Query first, then the stored/last selection; the immediate watcher writes
// the resolved id back into the URL so a refresh or a copied link keeps the
// same device even when the id only came from localStorage.
const fromQuery = route.query.device
if (typeof fromQuery === 'string' && fromQuery) selectDevice(fromQuery)

watch(
  () => route.query.device,
  (value) => {
    if (typeof value === 'string' && value && value !== selectedId.value) selectDevice(value)
  },
)

watch(
  selectedId,
  (id, prev) => {
    if (authView.value === 'ready' && id && prev && id !== prev) {
      void status.load()
    }
    if ((route.query.device ?? null) === (id ?? null)) return
    const query = { ...route.query }
    if (id) query.device = id
    else delete query.device
    void router.replace({ query })
  },
  { immediate: true },
)
</script>

<template>
  <template v-if="authView === 'ready'">
    <a class="skip" href="#main">{{ t('skipToContent') }}</a>
    <div class="shell">
    <header class="top">
      <div class="brand">
        <BrandLogo />
        <p class="brand__sub">{{ t('brandSub') }}</p>
      </div>

      <nav class="nav" :aria-label="t('brand')">
        <RouterLink
          v-for="link in links"
          :key="link.path"
          :to="navTo(link.path)"
          class="nav__link"
          active-class="nav__link--active"
        >
          <AppIcon :name="link.icon" :size="15" />
          <span>{{ t(link.key) }}</span>
        </RouterLink>
      </nav>

      <div class="tools">
        <Pill :tone="connTone" size="sm" mono>
          <AppIcon :name="connIcon" :size="13" />
          {{ connLabel }}
        </Pill>

        <label v-if="devices.length > 1" class="device-pick">
          <AppIcon name="device" :size="14" />
          <span class="sr-only">{{ t('deviceLabel') }}</span>
          <select :value="selected?.device ?? ''" @change="chooseDevice">
            <option v-for="device in devices" :key="device.device" :value="device.device">
              {{ deviceName(device.device)
              }}{{ deviceRoom(device.device) ? ` · ${deviceRoom(device.device)}` : '' }}
            </option>
          </select>
        </label>
        <Pill v-else-if="selected" size="sm">
          <AppIcon name="device" :size="13" />
          <span class="device-pick__name">{{ deviceName(selected.device) }}</span>
          <span v-if="deviceRoom(selected.device)" class="device-pick__room">
            · {{ deviceRoom(selected.device) }}
          </span>
        </Pill>

        <span class="tools__version mono" :title="`${t('appVersion')} ${version}`">v{{ version }}</span>
        <button type="button" class="tool-btn" :title="t('themeCycle')" @click="cycleTheme">
          <AppIcon :name="themeIcon" :size="15" />
          <span>{{ themeShort() }}</span>
        </button>
        <button
          type="button"
          class="tool-btn"
          :class="{ 'tool-btn--active': advancedMode }"
          :aria-pressed="advancedMode"
          :title="t('advancedHint')"
          @click="toggleAdvancedMode"
        >
          <AppIcon name="settings" :size="15" />
          <span>{{ t('advancedTitle') }}</span>
        </button>
        <button type="button" class="tool-btn" :title="t('appLanguage')" @click="switchLanguage">
          <AppIcon name="language" :size="15" />
          <span>{{ t('appSwitchLanguage') }}</span>
        </button>
      </div>
    </header>

    <ConfirmationNotice />

    <main id="main" class="main">
      <RouterView v-slot="{ Component }">
        <component :is="Component" />
      </RouterView>
    </main>

    <footer class="foot">
      <span>{{ t('brand') }} · {{ t('brandSub') }}</span>
      <span class="mono">{{ pageTitle }} · v{{ version }}</span>
    </footer>
  </div>
  <Toast />
  </template>
  <AuthGate v-else :state="authView" @ready="authView = 'ready'" @retry="bootAuth" />
</template>

<style scoped>
.skip {
  position: absolute;
  left: -999px;
  top: 0;
  background: var(--surface);
  color: var(--ink);
  padding: var(--sp-2) var(--sp-3);
  z-index: 60;
}

.skip:focus {
  left: var(--sp-3);
  top: var(--sp-3);
}

.shell {
  max-width: var(--shell-max);
  margin: 0 auto;
  padding: var(--sp-4) var(--sp-4) var(--sp-7);
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.top {
  display: flex;
  align-items: center;
  gap: var(--sp-4);
  flex-wrap: wrap;
  padding: var(--sp-3) 0;
  border-bottom: 1px solid var(--line);
}

.brand {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  min-width: 0;
}

.brand__sub {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.nav {
  display: flex;
  gap: var(--sp-1);
  flex-wrap: wrap;
  flex: 1 1 auto;
  justify-content: center;
}

.nav__link {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  color: var(--muted);
  border-radius: var(--r-pill);
  padding: 0.28rem var(--sp-3);
  font-size: var(--fs-sm);
  white-space: nowrap;
}

.nav__link:hover {
  background: var(--surface-2);
  color: var(--ink);
  text-decoration: none;
}

.nav__link--active {
  background: var(--accent-soft);
  color: var(--accent);
  font-weight: 600;
}

.tools {
  display: flex;
  margin-left: auto;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.tools__version {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.device-pick {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border: 1px solid var(--line);
  border-radius: var(--r-pill);
  background: var(--surface-2);
  color: var(--muted);
  padding: 0.05rem var(--sp-2);
}

.device-pick__name {
  max-width: 12rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.device-pick__room {
  color: var(--muted);
}

.device-pick select {
  background: transparent;
  border: 0;
  color: var(--ink-2);
  font-family: var(--font-mono);
  font-size: var(--fs-xs);
  padding: 0.1rem 0;
  cursor: pointer;
  max-width: 13rem;
}

.tool-btn {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border: 1px solid var(--line-strong);
  background: var(--surface);
  color: var(--ink-2);
  border-radius: var(--r-sm);
  padding: 0.2rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.tool-btn:hover {
  background: var(--surface-2);
}

.tool-btn--active {
  border-color: var(--accent-line);
  background: var(--accent-soft);
  color: var(--accent);
}

.main {
  min-height: 55vh;
}

.foot {
  display: flex;
  justify-content: space-between;
  gap: var(--sp-3);
  flex-wrap: wrap;
  border-top: 1px solid var(--line);
  padding-top: var(--sp-3);
  color: var(--muted);
  font-size: var(--fs-xs);
}

@media (max-width: 900px) {
  .nav {
    order: 3;
    flex-basis: 100%;
    justify-content: flex-start;
    overflow-x: auto;
  }
}
</style>
