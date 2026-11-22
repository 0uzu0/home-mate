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
  type DeviceMeta,
  type GatewayDevice,
  type GatewaySnapshot,
  type ModeDevice,
  type ModeSnapshot,
  type RingEvent,
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

export const wifiProvisioning = computed<WifiProvisioningNotice>(() => {
  const resolved = new Set<string>()
  for (const e of voiceLog.value) {
    const dev = typeof e.device === 'string' ? e.device : ''
    if (
      e.kind === 'wifi_state' &&
      (e.state === 'applied' || e.state === 'connected' || e.state === 'joined')
    ) {
      resolved.add(dev)
    } else if (e.kind === 'wifi_provisioning_requested' && !resolved.has(dev)) {
      return {
        requested: true,
        device: dev || null,
        action: typeof e.action === 'string' ? e.action : null,
        trigger: typeof e.trigger === 'string' ? e.trigger : 'mute_hold',
        ts_ms: e.ts_ms,
      }
    }
  }
  return { requested: false }
})

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

/**
 * Refresh both snapshots. `allSettled` keeps the half that answered: a mode
 * timeout must not blank the gateway panel, and vice versa.
 */
export async function refresh(): Promise<void> {
  const mine = ++sequence
  loading.value = true
  attemptedAt.value = Date.now()
  const [modeResult, gatewayResult, metaResult] = await Promise.allSettled([
    getMode(),
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

// The stored/selected id must survive a rename or a device going away: fall
// back to the first device the backend reports.
watch(
  devices,
  (list) => {
    if (!list.length) return
    if (!selectedId.value || !list.some((device) => device.device === selectedId.value)) {
      selectedId.value = list[0].device
    }
  },
  { immediate: true },
)

watch(selectedId, (id) => {
  try {
    if (id) localStorage.setItem(STORAGE_KEY, id)
    else localStorage.removeItem(STORAGE_KEY)
  } catch {
    /* private mode */
  }
})
