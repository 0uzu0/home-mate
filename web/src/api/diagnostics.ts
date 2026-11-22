import { authToken, clearAuthToken, markUnauthorized } from '../auth'
import {
  ApiError,
  codeFor,
  request,
  type GatewaySnapshot,
  type RequestOptions,
  type RingEvent,
  type ServiceStatus,
} from './client'
import type { ModeDevice } from './devices'

// ── One-click self check (R2-BM6) ───────────────────────────────────────

export interface SelfCheckStage {
  stage: string
  state: string
  detail: string
  evidence: Record<string, unknown>
}

export interface SelfCheckResult {
  verdict: 'ready' | 'degraded' | 'failed' | string
  verdict_stage?: string | null
  deep: boolean
  generated_ms: number
  note: string
  stages: SelfCheckStage[]
}

export const runSelfCheck = (deep: boolean, opts?: RequestOptions) =>
  request<SelfCheckResult>(
    '/api/v0/diagnostics/self_check',
    { method: 'POST', body: JSON.stringify({ deep }) },
    { timeoutMs: deep ? 90000 : 15000, ...opts },
  )

// ── Household memory & reminders ────────────────────────────────────────

export interface MemoryItem {
  id: string
  scope_person?: string
  scope_device?: string
  text: string
  kind: 'fact' | 'preference' | 'event' | 'alias'
  source: string
  pinned: boolean
  created_ms: number
  updated_ms: number
}

export interface MemoryList {
  items: MemoryItem[]
  count: number
  max: number
  max_text: number
  kinds: string[]
  error?: string
}

export interface Reminder {
  id: number
  device: string
  due_ms: number
  note: string
  created_ms: number
  fired_ms: number | null
  cancelled_ms: number | null
  state: 'pending' | 'fired' | 'cancelled'
}

export const getReminders = (opts?: RequestOptions) =>
  request<{ now_ms: number; reminders: Reminder[] }>(
    '/api/v0/reminders',
    {},
    { timeoutMs: 6000, ...opts },
  )

export const addReminder = (
  body: {
    note: string
    device?: string
    delay_minutes?: number
    hour?: number
    minute?: number
    day_offset?: number
  },
  opts?: RequestOptions,
) =>
  request<Reminder>('/api/v0/reminders', { method: 'POST', body: JSON.stringify(body) }, { timeoutMs: 6000, ...opts })

export const cancelReminder = (id: number, opts?: RequestOptions) =>
  request<Reminder>(`/api/v0/reminders/${id}`, { method: 'DELETE' }, { timeoutMs: 6000, ...opts })

export const getMemories = (opts?: RequestOptions) =>
  request<MemoryList>('/api/v0/memories', {}, { timeoutMs: 6000, ...opts })

export const addMemory = (
  body: { text: string; kind?: string; pinned?: boolean },
  opts?: RequestOptions,
) =>
  request<{ state: string; item: MemoryItem }>(
    '/api/v0/memories',
    { method: 'POST', body: JSON.stringify(body) },
    { timeoutMs: 6000, ...opts },
  )

export const updateMemory = (
  id: string,
  body: { text?: string; pinned?: boolean },
  opts?: RequestOptions,
) =>
  request<{ state: string; item: MemoryItem }>(
    `/api/v0/memories/${encodeURIComponent(id)}`,
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 6000, ...opts },
  )

export const deleteMemory = (id: string, opts?: RequestOptions) =>
  request<{ state: string; id: string }>(
    `/api/v0/memories/${encodeURIComponent(id)}`,
    { method: 'DELETE' },
    { timeoutMs: 6000, ...opts },
  )

export const clearMemories = (opts?: RequestOptions) =>
  request<{ state: string; deleted: number }>(
    '/api/v0/memories/clear',
    { method: 'POST' },
    { timeoutMs: 6000, ...opts },
  )

// ── #23 Long-Term Working Memory ────────────────────────────────────────

export interface WorkingMemoryItem {
  id: string
  scope_person: string
  scope_device: string
  key_or_topic: string
  content: string
  created_ms: number
  updated_ms: number
  pinned?: boolean
  access_count?: number
  last_accessed_ms?: number
  written_by_device?: string
  source_trace_id?: string
}

export interface WorkingMemoryList {
  items: WorkingMemoryItem[]
  count: number
  pinned_count?: number
  evicted_total?: number
  max: number
  max_key_len: number
  max_content_len: number
  error?: string
}

export const getWorkingMemory = (
  scopePerson?: string,
  scopeDevice?: string,
  opts?: RequestOptions,
) => {
  const params = new URLSearchParams()
  if (scopePerson?.trim()) params.set('scope_person', scopePerson.trim())
  if (scopeDevice?.trim()) params.set('scope_device', scopeDevice.trim())
  const qs = params.toString()
  return request<WorkingMemoryList>(
    `/api/v0/working-memory${qs ? `?${qs}` : ''}`,
    {},
    { timeoutMs: 6000, ...opts },
  )
}

export const putWorkingMemory = (
  body: {
    scope_person: string
    scope_device: string
    key_or_topic: string
    content: string
  },
  opts?: RequestOptions,
) =>
  request<{ state: string; item: WorkingMemoryItem }>(
    '/api/v0/working-memory',
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 6000, ...opts },
  )

export const deleteWorkingMemoryById = (id: string, opts?: RequestOptions) =>
  request<{ state: string; id: string }>(
    `/api/v0/working-memory/${encodeURIComponent(id)}`,
    { method: 'DELETE' },
    { timeoutMs: 6000, ...opts },
  )

export const clearWorkingMemory = (
  scopePerson?: string,
  scopeDevice?: string,
  opts?: RequestOptions,
) => {
  const params = new URLSearchParams()
  if (scopePerson?.trim()) params.set('scope_person', scopePerson.trim())
  if (scopeDevice?.trim()) params.set('scope_device', scopeDevice.trim())
  const qs = params.toString()
  return request<{ state: string; deleted: number }>(
    `/api/v0/working-memory/clear${qs ? `?${qs}` : ''}`,
    { method: 'POST' },
    { timeoutMs: 6000, ...opts },
  )
}

export const pinWorkingMemory = (id: string, pinned: boolean, opts?: RequestOptions) =>
  request<{ state: string; item: WorkingMemoryItem }>(
    `/api/v0/working-memory/${encodeURIComponent(id)}/pin`,
    { method: 'POST', body: JSON.stringify({ pinned }) },
    { timeoutMs: 6000, ...opts },
  )

export interface SharedMemoryStatus {
  enabled: boolean
  nas_base_url: string
  upload_enabled: boolean
  token_set: boolean
  backlog_count: number
  cache_count: number
  circuit_open: boolean
  open_until_ms: number
  consecutive_failures: number
  last_error?: string | null
  last_sync_ms?: number | null
}

export const getSharedMemoryStatus = (opts?: RequestOptions) =>
  request<SharedMemoryStatus>('/api/v0/shared-memory/status', {}, { timeoutMs: 6000, ...opts })

export const putSharedMemoryConfig = (
  body: { enabled: boolean; nas_base_url: string; upload_enabled: boolean; token?: string },
  opts?: RequestOptions,
) =>
  request<{ state: string; config: { enabled: boolean; nas_base_url: string; upload_enabled: boolean }; status: SharedMemoryStatus }>(
    '/api/v0/shared-memory/config',
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 6000, ...opts },
  )

export const clearSharedMemoryOutbox = (opts?: RequestOptions) =>
  request<{ state: string; deleted: number }>(
    '/api/v0/shared-memory/clear-outbox',
    { method: 'POST' },
    { timeoutMs: 6000, ...opts },
  )

export interface AgentMdRevision {
  id: number
  version: number
  content: string
  author: string
  note: string
  created_ms: number
}

export interface AgentMdSnapshot {
  content: string
  version: number
  updated_ms: number
  revisions: AgentMdRevision[]
}

export const getAgentMd = (opts?: RequestOptions) =>
  request<AgentMdSnapshot>('/api/v0/agent-md', {}, { timeoutMs: 6000, ...opts })

export const putAgentMd = (
  body: { content: string; note?: string },
  opts?: RequestOptions,
) =>
  request<{ state: string } & AgentMdSnapshot>(
    '/api/v0/agent-md',
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 6000, ...opts },
  )

export const rollbackAgentMd = (
  body: { id: number; note?: string },
  opts?: RequestOptions,
) =>
  request<{ state: string; restored_from: number } & AgentMdSnapshot>(
    '/api/v0/agent-md/rollback',
    { method: 'POST', body: JSON.stringify(body) },
    { timeoutMs: 6000, ...opts },
  )

export interface UserMdEntry {
  id: string
  person_id: string
  scope_device?: string
  key: string
  content: string
  source_device: string
  trace_id: string
  turn_ts: number
  rationale: string
  author: string
  status: 'active' | 'reverted' | string
  created_ms: number
  updated_ms: number
  reverted_ms?: number | null
  reverted_by: string
}

export const getUserMd = (status?: string, opts?: RequestOptions) => {
  const q = status ? `?status=${encodeURIComponent(status)}` : ''
  return request<{ entries: UserMdEntry[] }>(`/api/v0/user-md${q}`, {}, { timeoutMs: 6000, ...opts })
}

export const putUserMd = (
  body: { id?: string; person_id?: string; key: string; content: string; rationale?: string },
  opts?: RequestOptions,
) =>
  request<{ state: string; entry: UserMdEntry }>(
    '/api/v0/user-md',
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 6000, ...opts },
  )

export const revertUserMd = (id: string, opts?: RequestOptions) =>
  request<{ state: string; entry: UserMdEntry }>(
    `/api/v0/user-md/${encodeURIComponent(id)}/revert`,
    { method: 'POST' },
    { timeoutMs: 6000, ...opts },
  )

export interface MemoryCandidate {
  id: string
  scope_person?: string
  scope_device?: string
  text: string
  kind: 'fact' | 'preference' | 'event' | 'alias'
  status: 'pending' | 'confirmed' | 'rejected'
  device: string
  session_id: string
  reason: string
  source: string
  created_ms: number
  updated_ms: number
}

export interface MemoryCandidateList {
  items: MemoryCandidate[]
  count: number
  pending_max: number
  per_session_max: number
  ttl_ms: number
}

export const getMemoryCandidates = (status?: string, opts?: RequestOptions) =>
  request<MemoryCandidateList>(
    `/api/v0/memories/candidates${status ? `?status=${encodeURIComponent(status)}` : ''}`,
    {},
    { timeoutMs: 6000, ...opts },
  )

export const confirmMemoryCandidate = (id: string, opts?: RequestOptions) =>
  request<{ state: string; item: MemoryItem }>(
    `/api/v0/memories/candidates/${encodeURIComponent(id)}/confirm`,
    { method: 'POST' },
    { timeoutMs: 6000, ...opts },
  )

export const rejectMemoryCandidate = (id: string, opts?: RequestOptions) =>
  request<{ state: string; id: string }>(
    `/api/v0/memories/candidates/${encodeURIComponent(id)}/reject`,
    { method: 'POST' },
    { timeoutMs: 6000, ...opts },
  )

// ── Conversation (device session) ───────────────────────────────────────

export interface SessionMessage {
  role: string
  content: string
  ts_ms: number
}

export const getSessionMessages = (id: string, opts?: RequestOptions) =>
  request<{ messages: SessionMessage[]; session_id: string }>(
    `/api/v0/session/${encodeURIComponent(id)}/messages`,
    {},
    { timeoutMs: 6000, ...opts },
  )

export const getDeviceConversation = (device: string, opts?: RequestOptions, contextSessionId = '', page: { archiveBefore?: number; messageBefore?: number } = {}) => {
  const query = new URLSearchParams()
  if (contextSessionId) query.set('context_session_id', contextSessionId)
  if (page.archiveBefore !== undefined) query.set('archive_before', String(page.archiveBefore))
  if (page.messageBefore !== undefined) query.set('message_before', String(page.messageBefore))
  return request<{ session_id: string | null; messages: SessionMessage[]; current_context?: { generation: number } | null; selected_context?: { generation: number } | null; archived_contexts?: { session_id: string; generation: number }[]; next_archive_before?: number | null; next_message_before?: number | null; view?: string }>(
    `/api/v0/devices/${encodeURIComponent(device)}/conversation${query.size ? `?${query}` : ''}`, {}, { timeoutMs: 6000, ...opts },
  )
}

export const clearDeviceConversation = (device: string, expectedSessionId: string, opts?: RequestOptions) =>
  request<{ session_id: string; cleared: number }>(
    `/api/v0/devices/${encodeURIComponent(device)}/conversation`,
    { method: 'DELETE', body: JSON.stringify({ expected_session_id: expectedSessionId }) },
    { timeoutMs: 6000, ...opts },
  )

export const clearSession = (id: string, opts?: RequestOptions) =>
  request<{ session_id: string; cleared: number }>(
    `/api/v0/session/${encodeURIComponent(id)}/clear`,
    { method: 'POST' },
    { timeoutMs: 6000, ...opts },
  )

// ── Config history / backup ─────────────────────────────────────────────

export interface ConfigHistoryEntry {
  id: number
  ts_ms: number
  actor: string
  scope: string
  summary: string
  payload: Record<string, unknown>
}

export const getConfigHistory = (opts?: RequestOptions) =>
  request<{ history: ConfigHistoryEntry[] }>('/api/v0/config/history', {}, opts)

export const restoreConfigEntry = (id: number, opts?: RequestOptions) =>
  request<{ state: string; scope: string; note?: string }>(
    `/api/v0/config/history/${id}/restore`,
    { method: 'POST' },
    opts,
  )

export const getConfigExport = (secrets: boolean, opts?: RequestOptions) =>
  request<Record<string, unknown>>(
    `/api/v0/config/export${secrets ? '?with_secrets=true' : ''}`,
    secrets
      ? { method: 'POST', headers: { 'X-HM-Confirm-Secrets': 'EXPORT_SECRETS' } }
      : {},
    opts,
  )

export interface ConfigImportResult {
  state: string
  actor?: string
  applied: string[]
  warnings: string[]
}

export const importConfig = (doc: unknown, opts?: RequestOptions) =>
  request<ConfigImportResult>(
    '/api/v0/config/import',
    { method: 'POST', body: JSON.stringify(doc) },
    { timeoutMs: 20000, ...opts },
  )

// ── Fleet: peers and configuration push (M12a-1, v0.26) ─────────────────

export interface FleetNode {
  id: string
  base_url: string
  added_ms: number
  last_ok_ms: number
  last_error: string
  token_set: boolean
}

export interface FleetView {
  self_id: string
  self_id_source: 'config' | 'env' | 'device' | 'default'
  push_actor: string
  nodes: FleetNode[]
}

export interface FleetPingResult {
  id: string
  ok: boolean
  latency_ms?: number
  build?: string
  version?: string
  product?: string
  data_dir?: string
  auth_configured?: boolean
  http_status?: number
  error?: string
}

export interface FleetPushTargetResult {
  id: string
  ok: boolean
  dry_run?: boolean
  base_url?: string
  actor?: string
  applied: string[]
  warnings: string[]
  http_status?: number
  error?: string | null
}

export interface FleetPushResult {
  state: 'planned' | 'pushed'
  self_id: string
  actor: string
  include_secrets: boolean
  dry_run: boolean
  targets: string[]
  results: FleetPushTargetResult[]
  ok: number
  failed: number
}

export const getFleet = (opts?: RequestOptions) =>
  request<FleetView>('/api/v0/fleet', {}, { timeoutMs: 6000, ...opts })

export const setFleetSelfId = (selfId: string, opts?: RequestOptions) =>
  request<FleetView>(
    '/api/v0/fleet',
    { method: 'PUT', body: JSON.stringify({ self_id: selfId }) },
    { timeoutMs: 6000, ...opts },
  )

export const putFleetNode = (
  id: string,
  body: { base_url: string; token?: string; token_clear?: boolean },
  opts?: RequestOptions,
) =>
  request<{ state: string; node: FleetNode }>(
    `/api/v0/fleet/${encodeURIComponent(id)}`,
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 8000, ...opts },
  )

export const deleteFleetNode = (id: string, opts?: RequestOptions) =>
  request<{ state: string; id: string }>(
    `/api/v0/fleet/${encodeURIComponent(id)}`,
    { method: 'DELETE' },
    { timeoutMs: 6000, ...opts },
  )

export const pingFleet = (ids?: string[], opts?: RequestOptions) =>
  request<{ state: string; results: FleetPingResult[]; ok: number; failed: number }>(
    '/api/v0/fleet/ping',
    { method: 'POST', body: JSON.stringify(ids ? { ids } : {}) },
    { timeoutMs: 15000, ...opts },
  )

export const pushFleet = (
  body: { targets: string[]; include_secrets?: boolean; dry_run?: boolean },
  opts?: RequestOptions,
) =>
  request<FleetPushResult>(
    '/api/v0/fleet/push',
    { method: 'POST', body: JSON.stringify(body) },
    { timeoutMs: 45000, ...opts },
  )

// ── Diagnostics ─────────────────────────────────────────────────────────

export interface Diagnostics {
  services: Record<string, ServiceStatus>
  gateway: GatewaySnapshot
  devices: ModeDevice[]
  voice_counts: Record<string, number>
  gateway_counts: Record<string, number>
  recent_errors: RingEvent[]
  failed_traces: TraceMeta[]
  note: string
}

export const getDiagnostics = (opts?: RequestOptions) =>
  request<Diagnostics>('/api/v0/diagnostics', {}, opts)

export interface VoicePlayTestResult {
  ok: boolean
  device?: string
  stage?: string
  synth_ms?: number
  total_ms?: number
  duration_ms?: number
  provider?: string
  voice?: string
  play_id?: string
  error?: string
}

export const runVoicePlayTest = (opts?: RequestOptions) =>
  request<VoicePlayTestResult>(
    '/api/v0/diagnostics/tests/voice_play',
    { method: 'POST' },
    { timeoutMs: 45000, ...opts },
  )

export interface PlayAssetResult {
  ok: boolean
  device?: string
  asset?: string
  stage?: string
  channel?: string
  duration_ms?: number
  total_ms?: number
  play_id?: string
  error?: string
}

export const runPlayAssetTest = (asset: string, volume?: number, opts?: RequestOptions) =>
  request<PlayAssetResult>(
    '/api/v0/diagnostics/tests/play_asset',
    {
      method: 'POST',
      body: JSON.stringify({ asset, ...(volume !== undefined ? { volume } : {}) }),
    },
    { timeoutMs: 60000, ...opts },
  )

export interface LoopbackReport {
  verdict: string
  lag_samples: number | null
  correlation: number | null
  exact_run: number
  write_samples: number
  capture_samples: number
  write_clipped: number
  capture_clipped: number
  longest_zero_ms: number
}

export interface LoopbackResult {
  ok: boolean
  error?: string
  play?: PlayAssetResult
  loopback?: LoopbackReport
}

export const runPlaybackLoopbackTest = (asset?: string, opts?: RequestOptions) =>
  request<LoopbackResult>(
    '/api/v0/diagnostics/tests/playback_loopback',
    { method: 'POST', body: JSON.stringify(asset ? { asset } : {}) },
    { timeoutMs: 60000, ...opts },
  )

export const reapplyModeRepair = (device: string, opts?: RequestOptions) =>
  request<{ device: string; requested: string; state: string; note?: string }>(
    '/api/v0/diagnostics/repair/reapply_mode',
    { method: 'POST', body: JSON.stringify({ device }) },
    opts,
  )

// ── Traces & Chat ───────────────────────────────────────────────────────

export interface TraceMeta {
  id: string
  session_id: string
  task_id: string
  source: string
  wake_channel?: string | null
  wake_word?: string | null
  input_text: string
  status: string
  created_ms: number
  duration_ms: number
}

export interface TraceDetail extends TraceMeta {
  goal: string
  events: Array<{ seq: number; kind: string; payload: Record<string, unknown>; ts_ms: number }>
}

export const getTraces = (opts?: RequestOptions) =>
  request<{ traces: TraceMeta[] }>('/api/v0/trace?limit=50', {}, opts)

export const getTrace = (id: string, opts?: RequestOptions) =>
  request<TraceDetail>(`/api/v0/trace/${encodeURIComponent(id)}`, {}, opts)

export interface ChatEvent {
  type: string
  [k: string]: unknown
}

export interface ChatScopeBinding {
  binding_id: string
  session_id: string
  scope: { person_id?: string | null; device_id?: string | null; room?: string | null }
  household_write: boolean
  expires_ms: number
  ttl_ms: number
  data_epoch: number
}

export const createChatScope = (
  body: { person_id?: string; device_id?: string; household_write?: boolean }, opts?: RequestOptions,
) => request<ChatScopeBinding>('/api/v1/chat/scopes', { method: 'POST', body: JSON.stringify(body) }, opts)

export const getChatScope = (id: string, opts?: RequestOptions) =>
  request<ChatScopeBinding>(`/api/v1/chat/scopes/${encodeURIComponent(id)}`, {}, opts)

export const revokeChatScope = (id: string, opts?: RequestOptions) =>
  request<{ binding_id: string; state: 'revoked' }>(`/api/v1/chat/scopes/${encodeURIComponent(id)}`, { method: 'DELETE' }, opts)

export interface ChatOptions extends RequestOptions { scopeBindingId?: string }

export async function postChat(
  sessionId: string | null,
  text: string,
  onEvent: (ev: ChatEvent) => void,
  opts: ChatOptions = {},
): Promise<void> {
  const controller = new AbortController()
  const timer = window.setTimeout(() => controller.abort(), opts.timeoutMs ?? 120000)
  const onOuterAbort = (): void => controller.abort()
  opts.signal?.addEventListener('abort', onOuterAbort, { once: true })
  try {
    const resp = await fetch('/api/v0/chat', {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        ...(authToken.value ? { Authorization: `Bearer ${authToken.value}` } : {}),
      },
      body: JSON.stringify({ session_id: sessionId ?? undefined, text,
        ...(opts.scopeBindingId ? { scope_binding_id: opts.scopeBindingId } : {}) }),
      signal: controller.signal,
    })
    if (!resp.ok || !resp.body) {
      if (resp.status === 401) {
        clearAuthToken()
        markUnauthorized()
      }
      const detail = await resp.text().catch(() => '')
      throw new ApiError(codeFor(resp.status), detail || `chat failed (${resp.status})`, resp.status)
    }
    const reader = resp.body.getReader()
    const decoder = new TextDecoder()
    let buf = ''
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      buf += decoder.decode(value, { stream: true })
      let nl: number
      while ((nl = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, nl).trim()
        buf = buf.slice(nl + 1)
        if (!line.startsWith('data:')) continue
        const payload = line.slice(5).trim()
        try {
          onEvent(JSON.parse(payload) as ChatEvent)
        } catch {
          /* keep-alive or partial chunk */
        }
      }
    }
  } catch (err) {
    if (err instanceof ApiError) throw err
    if (controller.signal.aborted) throw new ApiError('aborted', 'chat aborted')
    throw new ApiError('network', err instanceof Error ? err.message : String(err))
  } finally {
    window.clearTimeout(timer)
    opts.signal?.removeEventListener('abort', onOuterAbort)
  }
}

export const cancelTask = (taskId: string, opts?: RequestOptions) =>
  request<{ state?: string }>(
    '/api/v0/session/cancel',
    { method: 'POST', body: JSON.stringify({ task_id: taskId }) },
    opts,
  )
