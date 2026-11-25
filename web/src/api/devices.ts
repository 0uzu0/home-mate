import {
  request,
  type GatewayDevice,
  type GatewaySnapshot,
  type RequestOptions,
  type RingEvent,
} from './client'
import type { HomeStatus } from './services'

export const getDevices = (opts?: RequestOptions) =>
  request<{ gateway: GatewaySnapshot; recent: RingEvent[]; voice_log: RingEvent[] }>(
    '/api/v0/devices',
    {},
    opts,
  )

export const stopDevice = (id: string, opts?: RequestOptions) =>
  request<{ device: string; note: string }>(
    `/api/v0/devices/${encodeURIComponent(id)}/stop`,
    { method: 'POST' },
    opts,
  )

export const setDeviceVolume = (id: string, value: number, opts?: RequestOptions) =>
  request<{ device: string; value: number; state: string }>(
    `/api/v0/devices/${encodeURIComponent(id)}/volume`,
    { method: 'POST', body: JSON.stringify({ value }) },
    opts,
  )

export const setDeviceMute = (id: string, muted: boolean, opts?: RequestOptions) =>
  request<{
    applied: boolean
    stage: string
    task_id: string
    device: string
    muted?: boolean
    source?: string
    detail?: string
  }>(
    `/api/v0/devices/${encodeURIComponent(id)}/mute`,
    { method: 'PUT', body: JSON.stringify({ muted }) },
    opts,
  )

export interface DeviceMeta {
  name: string
  room: string
}

export const getDeviceMeta = (opts?: RequestOptions) =>
  request<{ meta: Record<string, DeviceMeta> }>('/api/v0/device-meta', {}, opts)

export const putDeviceMeta = (
  device: string,
  body: { name: string; room: string },
  opts?: RequestOptions,
) =>
  request<{ state: string; device: string; meta: DeviceMeta }>(
    `/api/v0/device-meta/${encodeURIComponent(device)}`,
    { method: 'PUT', body: JSON.stringify(body) },
    opts,
  )

export interface UnitAcousticProfile {
  volume_cap: number
  mix_gain: number
  vad_threshold: number
  onset_threshold_dbfs?: number
  mic_gain_offset_db?: number
  mic_calibrated?: boolean
}

export interface AcousticsReceipt {
  state?: string
  block_generation?: number
  reason?: string | null
  error_code?: string | null
  detail?: unknown
}

export interface DeviceLedConfig {
  night_enabled: boolean
  night_start_minute: number
  night_end_minute: number
  night_brightness_pct: number
  orientation_rotation: number
  orientation_reverse: boolean
}

export interface DeviceProfile {
  device: string
  acoustic: UnitAcousticProfile
  led: DeviceLedConfig
  acoustics_state?: string
  acoustics_receipt?: AcousticsReceipt | null
}

export const getDeviceProfile = (id: string, opts?: RequestOptions) =>
  request<DeviceProfile>(`/api/v0/devices/${encodeURIComponent(id)}/profile`, {}, opts)

export const putDeviceProfile = (
  id: string,
  body: { acoustic?: Partial<UnitAcousticProfile>; led?: Partial<DeviceLedConfig> },
  opts?: RequestOptions,
) =>
  request<
    DeviceProfile & {
      state: string
      acoustics_state?: string
      acoustics_pushed?: boolean
      acoustics_receipt?: unknown
    }
  >(
    `/api/v0/devices/${encodeURIComponent(id)}/profile`,
    { method: 'PUT', body: JSON.stringify(body) },
    opts,
  )

export const restartCore = (opts?: RequestOptions) =>
  request<{ state: string }>('/api/v0/system/restart', { method: 'POST' }, opts)

export const factoryReset = (confirm: 'FACTORY_RESET', opts?: RequestOptions) =>
  request<{ state: string; secrets_preserved: boolean }>(
    '/api/v0/system/factory-reset',
    { method: 'POST', body: JSON.stringify({ confirm }) },
    opts,
  )

export const setDeviceWifi = (id: string, ssid: string, psk: string, opts?: RequestOptions) =>
  request<{ state: string; ssid: string; detail?: string | null }>(
    `/api/v0/devices/${encodeURIComponent(id)}/wifi`,
    { method: 'POST', body: JSON.stringify({ ssid, psk }) },
    { timeoutMs: 80000, ...opts },
  )

export interface MicTelemetry {
  session: boolean | null
  detail?: string | null
  in_speech: boolean
  idle_gated?: boolean
  last_chunk_ms?: number | null
  chunks_per_sec?: number | null
  blocks_seen?: number
  levels?: {
    count: number
    min_db: number
    mean_db: number
    p95_db: number
    max_db: number
  } | null
  vad: {
    start_db: number
    end_db: number
    effective_start_db?: number
    effective_end_db?: number
    noise_db?: number | null
    noise_margin_db?: number
    speak_margin_db?: number
    last_close?: { reason: 'quiet' | 'cap'; blocks: number } | null
  }
  counters: {
    speech_started: number
    segments: number
    segments_discarded: number
    close_quiet?: number
    close_cap?: number
    wakes: number
    followup_turns: number
    ignored_unwoken: number
    asr_failed: number
    self_echo_suppressed: number
    tail_suppressed?: number
    barge_tail_dropped?: number
    segment_empty?: number
    asr_skipped?: number
  }
}

export interface ModeProgram {
  cursor: number
  chars: number
  complete: boolean
  next: string
  since_ms?: number
}

export interface SessionSnapshot {
  state: string
  since_ms: number
  generation: number
  phase: string
  opened_by: string | null
  credits: number
  tail_deadline_ms: number | null
  close_reason: string | null
  last_denied: { reason: string; at_ms: number; hint?: string } | null
}

export interface InputSnapshot {
  state: 'idle' | 'unadmitted' | 'capture' | 'recognizing' | 'cancelled' | 'expired' | 'consumed' | 'rejected' | 'discarded'
  input_id?: number
  capture_limit_ms?: number
  lifetime_limit_ms?: number
  remaining_ms?: number
  slot_limit: number
}

export interface ModeDevice {
  input?: InputSnapshot
  input_window_open?: boolean
  media_guard?: { play_id: string; connection_generation: number; uncertain: boolean; last_media_end_ms: number | null; guard_remaining_ms: number } | null
  device: string
  duplex: { capability: string; requested: string; negotiated: string; effective: string }
  aec?: { owner: string; reference: string } | null
  muted?: boolean | null
  mute_source?: string | null
  participation: string
  session?: SessionSnapshot
  proactive_speech: boolean
  follow_window_tone?: boolean
  retreat_intent?: boolean
  wake_ack?: { enabled: boolean; phrases: string[] }
  filler?: {
    live: { play_id: string; scene: string; phrase: string } | null
    delay_ms: number
    mode?: FillerMode
  }
  stop_intents?: { continue_intents: number }
  program?: { live: ModeProgram | null; paused: ModeProgram | null }
  activity: string
  assistant_state?: string | null
  current_task?: string | null
  mic?: MicTelemetry
  kws?: {
    state?: string | null
    detail?: string | null
    restarts: number
    detections: number
    guarded: number
    stops: number
    wakes?: number
    ignored: number
    last_keyword_ms: number
    phrase: string
    epoch?: number
    detections_since_epoch?: number
    counted_since_ms?: number
    core_epoch?: number
    threshold?: number
    score?: number
    local_exit?: number
  } | null
  reflex?: {
    words: {
      text: string
      tokens: string
      intent: 'wake' | 'retreat' | 'pause' | 'continue' | 'next' | 'stop'
      enabled: boolean
      hits: number
      ignored: number
      text_hits?: number
    }[]
    deploy: {
      state: string
      hash: string
      keywords: number
      detail: string
      sent_ms: number
      acked_ms: number
      sent_hash?: string
      loaded_hash?: string | null
      loaded_ms?: number
      loaded_matches?: boolean | null
      retreat_words_sent?: number
      local_exit?: number
      local_exit_matches?: boolean
      intents_hash?: string
      intents_hash_installed?: string
      intents_hash_matches?: boolean
      local_exits?: number
      kws_threshold?: number
      kws_score?: number
      applied_threshold?: number
      applied_score?: number
      last_local_exit?: {
        keyword: string
        ts: number
        latency_ms: number
        channels: string[]
        detail?: string | null
      } | null
    } | null
    dedupe_ms: number
  }
  barge?: {
    stops: number
    ignored: number
    suppressed?: number
    last_barge_in_ms: number
    preroll_held?: boolean
    window_open?: boolean
    protect_ms?: number
    protected?: boolean
  } | null
  latency?: {
    samples: number
    synth_mean_ms: number
    confirm_mean_ms: number
    confirm_max_ms: number
  } | null
  media_trim_db?: number
  tail_blind_ms?: number
  wake_words?: string[]
  retreat_words?: string[]
  barge_in?: boolean
  barge_protect_ms?: number
  keyword_interrupt?: boolean
  follow_window_enabled?: boolean
  follow_window_ms?: number
  context_window_ms?: number
  followup_credits?: number | null
  kws_threshold?: number
  kws_score?: number
  asr_wake_fallback?: boolean
  proactive_asr_wake?: boolean
  wake_arbitration?: WakeArbitration['mode']
  has_device_override?: boolean
  outputs: Array<{
    play_id: string
    channel: string
    stage: string
    task?: string | null
    content_ref?: string | null
    started_ms?: number
  }>
}

export type FillerMode = 'radio' | 'interrupt'
export type FillerScene = 'thinking' | 'search' | 'device' | 'story'

export interface FillerConfig {
  delay_ms: number
  tool_delay_ms?: number
  mode?: FillerMode
  thinking: string[]
  search: string[]
  device: string[]
  story: string[]
}

export interface StopIntentConfig {
  enabled: boolean
  stop_words: string[]
  continue_words: string[]
}

export interface ModeSnapshot {
  scope?: 'global' | 'device'
  device?: string
  has_device_override?: boolean
  device_overrides?: Record<string, Partial<ModePatch>>
  assistant_name?: string
  persona_prompt?: string
  system_prompt_override?: string | null
  follow_window_enabled?: boolean
  follow_window_ms?: number
  context_window_ms?: number
  followup_credits?: number | null
  follow_window_tone?: boolean
  kws_threshold?: number
  kws_score?: number
  default_duplex: string
  wake_words: string[]
  retreat_words?: string[]
  retreat_ack?: string
  window_close_ack?: string
  truncation_notice?: string
  vad?: { start_db: number; end_db: number } | null
  spoken?: SpokenDeliveryConfig
  tail_blind_ms?: number
  media_trim_db?: number
  keyword_interrupt?: boolean
  barge_in?: boolean
  barge_protect_ms?: number
  asr_wake_fallback?: boolean
  proactive_asr_wake?: boolean
  wake_arbitration?: WakeArbitration
  sovereignty?: Array<{
    session_id: string
    room: string
    owner_device: string
    lease_until_ms: number
    active_voice_until_ms: number
    context_until_ms: number
  }>
  filler?: FillerConfig
  stop_intents?: StopIntentConfig
  keyword_phrase?: string
  home?: HomeStatus
  devices: ModeDevice[]
}

export const getMode = (opts?: RequestOptions & { device?: string }) => {
  const { device, ...reqOpts } = opts ?? {}
  const q = device?.trim() ? `?device=${encodeURIComponent(device.trim())}` : ''
  return request<ModeSnapshot>(`/api/v0/mode${q}`, {}, reqOpts)
}

export interface WakeArbitration {
  mode: 'off' | 'observe' | 'enforce'
  peers: number
}

export interface SpokenDeliveryConfig {
  max_sentences: number
  max_chars: number
  detail_max_sentences: number
  detail_max_chars: number
  detail_words?: string[]
  long_form_words?: string[]
  continue_hint: string
  resume_lead: string
  backstop_max_sentences: number
  backstop_max_chars: number
}

export interface ModePatch {
  assistant_name?: string
  persona_prompt?: string
  system_prompt_override?: string | null
  follow_window_enabled?: boolean
  follow_window_ms?: number
  context_window_ms?: number
  followup_credits?: number
  kws_threshold?: number
  kws_score?: number
  duplex_requested?: string
  proactive_speech?: boolean
  follow_window_tone?: boolean
  wake_ack?: { enabled: boolean; phrases: string[]; resynth?: boolean }
  retreat_intent?: boolean
  wake_words?: string[]
  retreat_words?: string[]
  retreat_ack?: string
  window_close_ack?: string
  truncation_notice?: string
  vad?: { start_db: number; end_db: number }
  tail_blind_ms?: number
  media_trim_db?: number
  keyword_interrupt?: boolean
  barge_in?: boolean
  barge_protect_ms?: number
  filler?: FillerConfig & { resynth?: boolean }
  stop_intents?: StopIntentConfig
  spoken?: SpokenDeliveryConfig
  asr_wake_fallback?: boolean
  proactive_asr_wake?: boolean
  wake_arbitration?: WakeArbitration['mode']
}

export const putMode = (device: string, patch: ModePatch, opts?: RequestOptions) =>
  request<ModeDevice & { scope?: string; reason?: string | null }>(
    `/api/v0/mode/${encodeURIComponent(device)}`,
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const putGlobalMode = (patch: ModePatch, opts?: RequestOptions) =>
  request<ModeSnapshot & { scope?: string; reason?: string | null }>(
    '/api/v0/mode',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

/**
 * Mirrors backend `diagnostics.rs:155-176` & `gateway/outbound.rs:1108-1216` (`MG-P2-03`):
 * mobile/offline-KWS gateways (explicit `idle_gated_*` capabilities or `android-v2`/`android-*`
 * mobile profile) intentionally pause uplink audio chunks while idle on local KWS.
 * Fixed LAN/Tailscale continuous speakers (`loopback: false`) are NEVER inferred as idle-gated.
 */
export function isDeviceIdleGated(
  device: ModeDevice | null | undefined,
  gatewayDevice?: GatewayDevice | null,
): boolean {
  if (!device) return false
  if (device.mic?.idle_gated === true) return true
  const online = gatewayDevice ? gatewayDevice.online : true
  const muted = device.muted === true
  const session = device.mic?.session === true
  const kwsRunning = device.kws?.state === 'running'
  const inSpeech = device.mic?.in_speech === true
  const sessionOpen = Boolean(device.participation && device.participation !== 'idle')
  const caps = gatewayDevice?.capabilities ?? []
  const hasIdleGateCap = caps.some(
    (c) => c === 'idle_gated_mic' || c === 'idle_gated_audio' || c === 'offline_kws_idle_gate',
  )
  const pv = (gatewayDevice?.profile_version ?? '').trim().toLowerCase()
  const isMobileProfile = pv === 'android-v2' || pv.startsWith('android-')
  return (
    (hasIdleGateCap || isMobileProfile) &&
    online &&
    !muted &&
    session &&
    kwsRunning &&
    !inSpeech &&
    !sessionOpen
  )
}

export interface VoiceEvent {
  kind: string
  ts_ms: number
  seq?: number
  epoch?: number
  [key: string]: unknown
}

export interface VoiceEventsPage {
  events: VoiceEvent[]
  next_seq?: number
  epoch?: number
}

export const getVoiceEvents = (
  since: number,
  limit = 500,
  opts?: RequestOptions & { tail?: boolean },
) => {
  const { tail, ...reqOpts } = opts ?? {}
  const tailParam = tail ? '&tail=true' : ''
  return request<VoiceEventsPage>(
    `/api/v0/voice/events?since=${since}&limit=${limit}${tailParam}`,
    {},
    { timeoutMs: 15000, ...reqOpts },
  )
}

export type AudioCurve = 'echomuse' | 'official' | 'rui' | 'custom'

export interface EqBandConfig {
  freq: number
  gain_db: number
  q: number
}

export interface AudioEqConfig {
  curve: AudioCurve
  bands: EqBandConfig[]
}

export interface AudioEqResponse extends AudioEqConfig {
  curves: AudioCurve[]
  default: AudioEqConfig
  voicing?: EqBandConfig[]
  devices: string[]
}

export const getAudioEq = (opts?: RequestOptions) =>
  request<AudioEqResponse>('/api/v0/config/audio', {}, opts)

export const putAudioEq = (cfg: AudioEqConfig, opts?: RequestOptions) =>
  request<{ state: string; curve: AudioCurve; bands: EqBandConfig[]; sent: string[]; errors: unknown[] }>(
    '/api/v0/config/audio',
    { method: 'PUT', body: JSON.stringify(cfg) },
    opts,
  )

export const pauseProgram = (device: string, opts?: RequestOptions) =>
  request<{ paused: boolean; bookmark: unknown }>(
    `/api/v0/devices/${encodeURIComponent(device)}/program/pause`,
    { method: 'POST' },
    opts,
  )

export const resumeProgram = (device: string, opts?: RequestOptions) =>
  request<{ resumed: boolean; bookmark: unknown }>(
    `/api/v0/devices/${encodeURIComponent(device)}/program/resume`,
    { method: 'POST' },
    opts,
  )

export interface ReflexWord {
  text: string
  tokens: string
  intent: 'wake' | 'retreat' | 'pause' | 'continue' | 'next' | 'stop'
  enabled: boolean
}

export interface ReflexState {
  words: ReflexWord[]
  compiled: string
  hash: string
  error?: string | null
  warnings?: string[]
}

export const getReflexTokens = (text: string, opts?: RequestOptions) =>
  request<{ text: string; tokens: string; error: string | null }>(
    `/api/v0/reflex/tokens?text=${encodeURIComponent(text)}`,
    {},
    opts,
  )

export const getReflex = (opts?: RequestOptions) =>
  request<ReflexState>('/api/v0/reflex', {}, opts)

export const putReflex = (
  body: { words: ReflexWord[]; deploy?: boolean; device?: string },
  opts?: RequestOptions,
) =>
  request<ReflexState & { deploy?: unknown; state: string }>(
    '/api/v0/reflex',
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 35000, ...opts },
  )

export const deployReflex = (body: { device?: string } = {}, opts?: RequestOptions) =>
  request<{ state: string; hash: string; words: number; device: string; keywords_file: string }>(
    '/api/v0/reflex/deploy',
    { method: 'POST', body: JSON.stringify(body) },
    { timeoutMs: 35000, ...opts },
  )

export interface FirmwareSlotInfo {
  name: string
  size: number
  version?: string | null
  sha8: string
}

export interface FirmwareLayout {
  available: boolean
  dir: string
  active_slot?: string | null
  slots: FirmwareSlotInfo[]
}

export interface FirmwareStatus {
  audio: FirmwareLayout
  core: FirmwareLayout
}

export const getFirmware = (opts?: RequestOptions) =>
  request<FirmwareStatus>('/api/v0/firmware', {}, opts)

export interface FirmwareStageResult {
  staged_slot: string
  version?: string | null
  size: number
  restarted?: boolean
  restarting?: boolean
}

export const updateFirmwareAudio = (file: File, restart = true, opts?: RequestOptions) => {
  const form = new FormData()
  form.append('file', file)
  return request<FirmwareStageResult>(
    `/api/v0/firmware/audio?restart=${restart}`,
    { method: 'POST', body: form },
    { timeoutMs: 120000, ...opts },
  )
}

export const updateFirmwareCore = (file: File, opts?: RequestOptions) => {
  const form = new FormData()
  form.append('file', file)
  return request<FirmwareStageResult>(
    '/api/v0/firmware/core',
    { method: 'POST', body: form },
    { timeoutMs: 120000, ...opts },
  )
}

export const closeSession = (device: string, generation?: number, opts?: RequestOptions) =>
  request<{ session: SessionSnapshot; closed: boolean }>('/api/v0/session/close',
    { method: 'POST', body: JSON.stringify({ device, generation }) }, opts)

export const putPrompts = (patch: Pick<ModePatch, 'assistant_name' | 'persona_prompt' | 'system_prompt_override'>, opts?: RequestOptions) =>
  request<Pick<ModeSnapshot, 'assistant_name' | 'persona_prompt' | 'system_prompt_override'>>('/api/v0/prompts',
    { method: 'PUT', body: JSON.stringify(patch) }, opts)
