/**
 * Shared device/mode snapshot.
 *
 * There is exactly one poll for the whole app (owned by AppShell) and one
 * selected device. Every page reads the same snapshot, so a save on one page
 * is visible on the next without a second request, and a second Dot would
 * appear in the selector without a page changing.
 *
 * Selection is persisted per browser and can be carried in the URL as
 * `?device=<stable_id>`; the shell keeps the two in sync.
 */
import { computed, ref, shallowRef, watch } from 'vue'
import {
  getDeviceMeta,
  getDevices,
  getMode,
  resolveWifiProvisioningForDevice,
  type DeviceMeta,
  type GatewayDevice,
  type GatewaySnapshot,
  type ModeDevice,
  type ModeSnapshot,
  type RingEvent,
  type Status,
  type WifiProvisioningNotice,
} from '../api'

const STORAGE_KEY = 'hm.device'

function readStoredId(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY)
  } catch {
    return null
  }
}

export const mode = shallowRef<ModeSnapshot | null>(null)
export const gateway = shallowRef<GatewaySnapshot | null>(null)
export const statusSnapshot = shallowRef<Status | null>(null)
export const voiceLog = shallowRef<RingEvent[]>([])
export const meta = shallowRef<Record<string, DeviceMeta>>({})
export const modeError = shallowRef<unknown>(null)
export const gatewayError = shallowRef<unknown>(null)
export const metaError = shallowRef<unknown>(null)
export const metaLoaded = ref(false)
export const loading = ref(false)
/** Attempt time never represents the age of retained data. */
export const attemptedAt = ref<number | null>(null)
export const modeUpdatedAt = ref<number | null>(null)
export const gatewayUpdatedAt = ref<number | null>(null)
export const metaUpdatedAt = ref<number | null>(null)
export const selectedId = ref<string | null>(readStoredId())

export const devices = computed<ModeDevice[]>(() => mode.value?.devices ?? [])
export const gatewayDevices = computed<GatewayDevice[]>(() => gateway.value?.devices ?? [])
export const error = computed(() => modeError.value ?? gatewayError.value)

export const wifiProvisioningByDevice = computed<Record<string, WifiProvisioningNotice>>(() => {
  const map: Record<string, WifiProvisioningNotice> = {}
  const fromStatus = statusSnapshot.value?.wifi_provisioning_by_device
  if (fromStatus && typeof fromStatus === 'object') {
    for (const [dev, notice] of Object.entries(fromStatus)) {
      if (dev && notice?.requested) map[dev] = notice
    }
  }
  const resolved = new Set<string>()
  for (const e of voiceLog.value) {
    const dev = typeof e.device === 'string' ? e.device.trim() : ''
    if (!dev) continue
    if (
      e.kind === 'wifi_state' &&
      (e.state === 'applied' ||
        e.state === 'connected' ||
        e.state === 'joined' ||
        e.state === 'associated')
    ) {
      resolved.add(dev)
    } else if (e.kind === 'wifi_provisioning_requested' && !resolved.has(dev) && !map[dev]) {
      map[dev] = {
        requested: true,
        device: dev,
        action: typeof e.action === 'string' ? e.action : null,
        trigger: typeof e.trigger === 'string' ? e.trigger : 'mute_hold',
        ts_ms: e.ts_ms,
      }
    }
  }
  return map
})

export const wifiProvisioning = computed<WifiProvisioningNotice>(() =>
  resolveWifiProvisioningForDevice(
    selected.value?.device ?? selectedId.value,
    statusSnapshot.value,
    wifiProvisioningByDevice.value,
    devices.value.length || gatewayDevices.value.length,
  ),
)

export function setStatusSnapshot(next: Status | null): void {
  statusSnapshot.value = next
}

/** Existing room names, for the datalist in the device-label editor. */
export const rooms = computed(() =>
  [...new Set(Object.values(meta.value).map((item) => item.room).filter(Boolean))].sort((a, b) =>
    a.localeCompare(b),
  ),
)

/** Display name; the stable id is the honest fallback. */
export function deviceName(id: string): string {
  return meta.value[id]?.name?.trim() || id
}

export function deviceRoom(id: string): string {
  return meta.value[id]?.room?.trim() || ''
}

export const selected = computed<ModeDevice | null>(() => {
  const list = devices.value
  return list.find((device) => device.device === selectedId.value) ?? list[0] ?? null
})

export const selectedGateway = computed<GatewayDevice | null>(() => {
  // Exact match only: a fallback to the first gateway device would send
  // per-device actions (volume) to the wrong Dot when the selected device has
  // no gateway record yet.
  return gatewayDevices.value.find((device) => device.stable_id === selected.value?.device) ?? null
})

export function gatewayFor(deviceId: string): GatewayDevice | null {
  return gatewayDevices.value.find((device) => device.stable_id === deviceId) ?? null
}

export function selectDevice(id: string): void {
  selectedId.value = id
}

let sequence = 0
let missingSelectedTicks = 0
const seenDevices = new Set<string>()
const MISSING_TICKS_BEFORE_FALLBACK = 3

async function doRefresh(silentRequested: boolean): Promise<void> {
  const mine = ++sequence
  const silent = silentRequested && mode.value !== null
  if (!silent) loading.value = true
  attemptedAt.value = Date.now()
  const targetDevice = selectedId.value?.trim() || undefined
  const [modeResult, gatewayResult, metaResult] = await Promise.allSettled([
    getMode(targetDevice ? { device: targetDevice } : undefined),
    getDevices(),
    getDeviceMeta(),
  ])
  if (mine !== sequence) return
  if (modeResult.status === 'fulfilled') {
    mode.value = modeResult.value
    modeUpdatedAt.value = Date.now()
    modeError.value = null
  } else {
    modeError.value = modeResult.reason
  }
  if (gatewayResult.status === 'fulfilled') {
    gateway.value = gatewayResult.value.gateway
    gatewayUpdatedAt.value = Date.now()
    voiceLog.value = gatewayResult.value.voice_log ?? []
    gatewayError.value = null
  } else {
    gatewayError.value = gatewayResult.reason
  }
  if (metaResult.status === 'fulfilled') {
    meta.value = metaResult.value.meta ?? {}
    metaUpdatedAt.value = Date.now()
    metaError.value = null
    metaLoaded.value = true
  } else {
    // Labels are presentation metadata: their failure must not blank the
    // device pages, so it is kept separate from `error`.
    metaError.value = metaResult.reason
  }
  loading.value = false
}

/**
 * Refresh both snapshots. `allSettled` keeps the half that answered: a mode
 * timeout must not blank the gateway panel, and vice versa.
 */
export function refresh(): Promise<void> {
  return doRefresh(false)
}

/** Silent 500ms background poll: does not flip `loading` after the initial load. */
export function refreshSilent(): Promise<void> {
  return doRefresh(true)
}

// The stored/selected id must survive a rename or a device going away: fall
// back to the first device the backend reports, while tolerating transient
// single-tick dropouts for a device already seen in this session.
watch(
  devices,
  (list) => {
    if (!list.length) return
    for (const device of list) seenDevices.add(device.device)
    if (!selectedId.value) {
      missingSelectedTicks = 0
      selectedId.value = list[0].device
      return
    }
    if (list.some((device) => device.device === selectedId.value)) {
      missingSelectedTicks = 0
      return
    }
    if (!seenDevices.has(selectedId.value)) {
      missingSelectedTicks = 0
      selectedId.value = list[0].device
      return
    }
    missingSelectedTicks += 1
    if (missingSelectedTicks >= MISSING_TICKS_BEFORE_FALLBACK) {
      missingSelectedTicks = 0
      selectedId.value = list[0].device
    }
  },
  { immediate: true },
)

watch(selectedId, (id, prev) => {
  missingSelectedTicks = 0
  try {
    if (id) localStorage.setItem(STORAGE_KEY, id)
    else localStorage.removeItem(STORAGE_KEY)
  } catch {
    /* private mode */
  }
  if (id && prev && id !== prev && mode.value) {
    void refresh()
  }
})
