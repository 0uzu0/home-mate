import { authToken, clearAuthToken, markUnauthorized } from '../auth'

export type ApiErrorCode =
  | 'invalid_request'
  | 'unauthorized'
  | 'forbidden'
  | 'not_found'
  | 'conflict'
  | 'cooldown'
  | 'server_error'
  | 'network'
  | 'timeout'
  | 'aborted'
  | 'bad_response'

export class ApiError extends Error {
  readonly code: ApiErrorCode
  readonly detail: string
  readonly status: number
  readonly serverCode?: string

  constructor(code: ApiErrorCode, detail: string, status = 0, serverCode?: string) {
    super(detail || code)
    this.name = 'ApiError'
    this.code = code
    this.detail = detail
    this.status = status
    this.serverCode = serverCode
  }
}

export function isDeviceRequiredError(err: unknown): boolean {
  if (err instanceof ApiError) {
    return err.serverCode === 'DEVICE_REQUIRED' || err.detail.includes('DEVICE_REQUIRED')
  }
  if (err && typeof err === 'object') {
    const rec = err as { code?: unknown; error?: unknown; detail?: unknown }
    if (rec.code === 'DEVICE_REQUIRED') return true
    if (typeof rec.error === 'string' && rec.error.includes('DEVICE_REQUIRED')) return true
    if (typeof rec.detail === 'string' && rec.detail.includes('DEVICE_REQUIRED')) return true
  }
  return false
}

export function isAbort(err: unknown): boolean {
  return err instanceof ApiError && err.code === 'aborted'
}

export function codeFor(status: number): ApiErrorCode {
  if (status === 400 || status === 422) return 'invalid_request'
  if (status === 401) return 'unauthorized'
  if (status === 403) return 'forbidden'
  if (status === 404) return 'not_found'
  if (status === 409) return 'conflict'
  if (status === 429) return 'cooldown'
  if (status >= 500) return 'server_error'
  return 'bad_response'
}

export interface RequestOptions {
  signal?: AbortSignal
  timeoutMs?: number
}

const DEFAULT_TIMEOUT_MS = 8000

function isErrorEnvelope(v: unknown): v is { error: string } {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return false
  const err = (v as { error?: unknown }).error
  if (typeof err !== 'string' || !err.trim()) return false
  return Object.keys(v).every((k) => k === 'error' || k === 'detail')
}

/** fetch + JSON with a timeout and the uniform error shape. */
export async function request<T>(path: string, init: RequestInit = {}, opts: RequestOptions = {}): Promise<T> {
  const controller = new AbortController()
  const timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS
  let timedOut = false
  const timer = window.setTimeout(() => {
    timedOut = true
    controller.abort()
  }, timeoutMs)

  let outerAborted = false
  const onOuterAbort = (): void => {
    outerAborted = true
    controller.abort()
  }
  if (opts.signal) {
    if (opts.signal.aborted) {
      window.clearTimeout(timer)
      throw new ApiError('aborted', 'request aborted')
    }
    opts.signal.addEventListener('abort', onOuterAbort, { once: true })
  }

  let response: Response
  try {
    response = await fetch(path, {
      ...init,
      signal: controller.signal,
      headers: {
        Accept: 'application/json',
        ...(init.body instanceof FormData ? {} : { 'Content-Type': 'application/json' }),
        ...(authToken.value ? { Authorization: `Bearer ${authToken.value}` } : {}),
        ...(init.headers ?? {}),
      },
    })
  } catch (err) {
    window.clearTimeout(timer)
    opts.signal?.removeEventListener('abort', onOuterAbort)
    if (timedOut) throw new ApiError('timeout', `timeout after ${timeoutMs}ms`)
    if (outerAborted) throw new ApiError('aborted', 'request aborted')
    throw new ApiError('network', err instanceof Error ? err.message : String(err))
  }

  try {
    const text = await response.text()
    if (!response.ok) {
      if (response.status === 401) {
        clearAuthToken()
        markUnauthorized()
      }
      let detail = ''
      let serverCode: string | undefined
      try {
        const body = JSON.parse(text) as { error?: unknown; detail?: unknown; code?: unknown }
        detail =
          typeof body.error === 'string' ? body.error : typeof body.detail === 'string' ? body.detail : ''
        if (typeof body.code === 'string' && body.code.trim()) serverCode = body.code.trim()
      } catch {
        detail = text.trim().slice(0, 300)
      }
      throw new ApiError(codeFor(response.status), detail, response.status, serverCode)
    }
    if (response.status === 204 || !text) return undefined as T
    let parsed: unknown
    try {
      parsed = JSON.parse(text) as unknown
    } catch {
      throw new ApiError('bad_response', 'response was not JSON', response.status)
    }
    if (isErrorEnvelope(parsed)) throw new ApiError('server_error', parsed.error, response.status)
    return parsed as T
  } catch (err) {
    if (err instanceof ApiError) throw err
    if (timedOut) throw new ApiError('timeout', `timeout after ${timeoutMs}ms`)
    if (outerAborted) throw new ApiError('aborted', 'request aborted')
    throw new ApiError('network', err instanceof Error ? err.message : String(err))
  } finally {
    window.clearTimeout(timer)
    opts.signal?.removeEventListener('abort', onOuterAbort)
  }
}

// ── Status / auth / release ─────────────────────────────────────────────

export interface ServiceStatus {
  provider: string
  base_url: string
  model: string
  api_key_env: string
  api_key_set: boolean
  configured: boolean
  voice?: string
  thinking?: string
}

export type ServiceName = 'llm' | 'llm_fallback' | 'asr' | 'tts'

export interface GatewayDevice {
  stable_id: string
  model: string
  firmware?: string | null
  capabilities: string[]
  audio_profile: string
  online: boolean
  loopback?: boolean
  generation: number
  last_seq: number
  wifi_ssid?: string | null
  volume?: number | null
  volume_at_ms?: number | null
  aec_tail_ms?: number | null
  profile_version?: string | null
  echo_health?: string | null
  tail_mismatch?: boolean
  northbound?: NorthboundReadback
}

export interface NorthboundStatus {
  schema: 1
  boot_id: string
  link_generation: number
  sampled_uptime_ms: number
  critical_current_count: number
  critical_high_water_count: number
  critical_current_bytes: number
  critical_high_water_bytes: number
  health_coalesced: number
  presence_coalesced: number
  critical_full: number
  invalid_frames: number
  audio_dropped: number
  audio_current_count: number
  audio_high_water_count: number
  writer_timeouts: number
  writer_errors: number
  transport_disconnects: number
  oldest_pending_ms: number
  oldest_pending_high_water_ms: number
  successful_write_bytes: number
  last_fault: { code: string; uptime_ms: number } | null
}
export interface NorthboundReadback {
  state: 'available' | 'absent' | 'unsupported' | 'invalid'
  schema: number | null
  sample: NorthboundStatus | null
  received_ms: number | null
  connection_generation: number | null
  stale: boolean
}

export interface GatewayConnection {
  generation: number
  gateway: string
  gateway_version?: string | null
  protocol_operating: number
  peer_addr?: string | null
  authenticated: boolean
  devices: string[]
}

export interface GatewaySnapshot {
  connected: boolean
  multi_gateway?: boolean
  voice_gateway_open?: boolean
  connection_count?: number
  connections?: GatewayConnection[]
  gateway?: string | null
  gateway_version?: string | null
  protocol_operating?: number | null
  last_link_error?: { kind: string; reason: string; ts_ms?: number } | null
  generation: number
  bind_addr: string
  devices: GatewayDevice[]
  pending_plays: number
  active_plays?: number
  pending_stop_sent?: number
  pending_stop_plays?: number
  pending_stop_waiters?: number
  pending_wifi?: number
  pending_volume?: number
  transport_tasks?: { connection_tasks: number; reader_futures: number; writer_tasks: number }
}

export interface RingEvent {
  kind: string
  ts_ms: number
  [k: string]: unknown
}

export interface WifiProvisioningNotice {
  requested: boolean
  device?: string | null
  action?: string | null
  trigger?: string | null
  ts_ms?: number | null
}

export interface Status {
  product: string
  assistant_name: string
  now_ms?: number
  version: string
  services: Record<string, ServiceStatus>
  gateway: GatewaySnapshot
  auth?: { configured: boolean; tokens: number; loopback_trusted: boolean }
  vault?: {
    ok: boolean
    sealed: number
    plaintext: number
    undecryptable: number
    key_path: string | null
  }
  wifi_provisioning?: WifiProvisioningNotice
  wifi_provisioning_by_device?: Record<string, WifiProvisioningNotice>
}

export const getStatus = (opts?: RequestOptions & { device?: string }) => {
  const { device, ...reqOpts } = opts ?? {}
  const q = device?.trim() ? `?device=${encodeURIComponent(device.trim())}` : ''
  return request<Status>(`/api/v0/status${q}`, {}, reqOpts)
}

/**
 * Resolve Wi-Fi provisioning status strictly for the selected device.
 *
 * Reads `wifi_provisioning_by_device[selectedDevice]` first, falling back to
 * legacy top-level `wifi_provisioning` ONLY when single-device or matching
 * `selectedDevice` so sibling devices never bleed into the active view.
 */
export function resolveWifiProvisioningForDevice(
  selectedDevice: string | null | undefined,
  status: Pick<Status, 'wifi_provisioning' | 'wifi_provisioning_by_device' | 'gateway'> | null | undefined,
  fallbackByDevice?: Record<string, WifiProvisioningNotice> | null,
  deviceCount?: number,
): WifiProvisioningNotice {
  const dev = selectedDevice?.trim() || ''
  const totalDevices = deviceCount ?? status?.gateway?.devices?.length ?? 0
  const map = status?.wifi_provisioning_by_device

  if (map && typeof map === 'object') {
    if (dev) {
      const entry = map[dev]
      if (entry?.requested) return entry
      // Map is authoritative when present; fall back to local voiceLog map for
      // the SAME device only, never to a sibling device.
      const localEntry = fallbackByDevice?.[dev]
      return localEntry?.requested ? localEntry : { requested: false, device: dev }
    }
    if (totalDevices <= 1) {
      const only = Object.values(map).find((entry) => entry?.requested)
      if (only) return only
    }
  }

  const top = status?.wifi_provisioning
  if (top?.requested) {
    const topDev = top.device?.trim() || ''
    if (!dev && totalDevices <= 1) return top
    if (dev && (topDev === dev || (!topDev && totalDevices <= 1))) return top
  }

  if (fallbackByDevice) {
    if (dev) {
      const entry = fallbackByDevice[dev]
      if (entry?.requested) return entry
      return { requested: false, device: dev }
    }
    if (totalDevices <= 1) {
      const only = Object.values(fallbackByDevice).find((entry) => entry?.requested)
      if (only) return only
    }
  }

  return { requested: false, device: dev || null }
}

export interface AuthTokenInfo {
  id: string
  label: string
  created_ms: number
  last_used_ms: number
}

export interface AuthStatus {
  configured: boolean
  corrupt: boolean
  core_id?: string
  lan_gateway_exempt?: boolean
  voice_gateway_open?: boolean
  tokens: AuthTokenInfo[]
  loopback_trusted: boolean
  detail?: string
}

export interface AuthLanExemptResponse {
  lan_gateway_exempt: boolean
  core_id: string
}

export interface AuthIssued {
  token: string
  created_ms: number
  note: string
}

export const getAuthStatus = (opts?: RequestOptions) =>
  request<AuthStatus>('/api/v0/auth', {}, opts)

export const provisionAuth = (opts?: RequestOptions) =>
  request<AuthIssued>('/api/v0/auth/provision', { method: 'POST' }, opts)

export const rotateAuth = (opts?: RequestOptions) =>
  request<AuthIssued>('/api/v0/auth/rotate', { method: 'POST' }, opts)

export const setLanGatewayExempt = (lan_gateway_exempt: boolean, opts?: RequestOptions) =>
  request<AuthLanExemptResponse>(
    '/api/v0/auth/lan-exempt',
    { method: 'PUT', body: JSON.stringify({ lan_gateway_exempt }) },
    opts,
  )

export const LED_STATES = [
  'idle',
  'listening',
  'hearing',
  'wake',
  'thinking',
  'speaking',
  'music',
  'paused',
  'muted',
  'error',
] as const

export type LedState = (typeof LED_STATES)[number]

export function ledStateOf(value: string | null | undefined): LedState {
  return (LED_STATES as readonly string[]).includes(value ?? '') ? (value as LedState) : 'idle'
}

export interface ReleaseRef {
  id: string
  serial: number
  version: string
  build_id?: string
  firmware_version?: string
  firmware_build_id?: string
  gateway_version?: string
  console_index?: string
  model_sha256?: string
  model?: string
  installed_ms?: number
  activated_ms?: number
}

export interface ReleaseCheck {
  name: string
  ok: boolean
  detail: string
}

export interface ReleaseDbWork {
  snapshot?: string | null
  schema_before?: number | null
  schema_after?: number | null
  restored?: boolean
  note?: string
}

export interface ReleaseRun {
  kind: string
  status: string
  target?: ReleaseRef | null
  from?: ReleaseRef | null
  started_ms: number
  ended_ms?: number
  criteria: ReleaseCheck[]
  error?: string | null
  db?: ReleaseDbWork | null
}

export interface ReleaseState {
  schema?: number
  active?: ReleaseRef | null
  previous?: ReleaseRef | null
  last?: ReleaseRun | null
  history?: ReleaseRun[]
  installer?: { version: string; binary: string; updated_ms: number } | null
}

export interface ReleaseView {
  installer: { path: string; present: boolean }
  releases_dir: string
  state: ReleaseState | null
  live: { build: { version: string; id: string }; schema: number | null; data_dir: string }
}

export const getRelease = (opts?: RequestOptions) => request<ReleaseView>('/api/v0/release', {}, opts)

export const installRelease = (url: string, activate: boolean, opts?: RequestOptions) =>
  request<{ state: string; pid: number; activate: boolean }>(
    '/api/v0/release/install',
    { method: 'POST', body: JSON.stringify({ url, activate }) },
    opts,
  )

export const activateRelease = (id: string | null, reason: string, opts?: RequestOptions) =>
  request<{ state: string; pid: number }>(
    '/api/v0/release/activate',
    { method: 'POST', body: JSON.stringify({ id, reason }) },
    opts,
  )

export const rollbackRelease = (reason: string, opts?: RequestOptions) =>
  request<{ state: string; pid: number }>(
    '/api/v0/release/rollback',
    { method: 'POST', body: JSON.stringify({ reason }) },
    opts,
  )

export const getReleaseLog = (lines = 200, opts?: RequestOptions) =>
  request<{ lines: string[]; path: string }>(`/api/v0/release/log?lines=${lines}`, {}, opts)
