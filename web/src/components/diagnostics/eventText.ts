/**
 * Human-readable rendering of voice-pipeline / gateway ring events. Unknown
 * kinds fall back to a compact JSON dump rather than being hidden — an
 * unlabelled fact is still a fact.
 */
import type { RingEvent } from '../../api'
import { t, type Key } from '../../i18n'

const KIND_KEYS: Record<string, Key> = {
  mic_wake: 'evMicWake',
  mic_utterance: 'evMicUtterance',
  mic_ignored_unwoken: 'evMicIgnored',
  mic_segment_closed: 'evMicSegment',
  mic_speech_started: 'evMicSpeech',
  device_status: 'evDeviceStatus',
  assistant_state: 'evAssistantState',
  tts_played: 'evTtsPlayed',
  play_stage: 'evPlayStage',
  play_finished: 'evPlayStage',
  button: 'evButton',
  button_stop: 'evButton',
  barge_in_stopped_plays: 'evBargeIn',
  barge_in_cancelled_task: 'evBargeIn',
  barge_in_stop: 'evBargeIn',
  barge_in_ignored: 'evBargeIn',
  barge_in_preroll_fed: 'evBargeIn',
  barge_in_preroll_skipped: 'evBargeIn',
  turn_end: 'evTurnEnd',
  volume_stage: 'evVolume',
  volume_set: 'evVolume',
  mute_state: 'evMute',
  mic_report: 'evMicReport',
  mode_set: 'evModeSet',
  keyword: 'evKeyword',
  keyword_stop: 'evKeywordStop',
  keyword_ignored: 'evKeywordIgnored',
  kws_report: 'evKwsReport',
}

export function kindLabel(kind: string): string {
  const key = KIND_KEYS[kind]
  return key ? t(key) : kind
}

export function formatEvent(e: RingEvent): string {
  const s = (v: unknown): string => (typeof v === 'string' ? v : '')
  const n = (v: unknown): string => (typeof v === 'number' ? String(v) : '—')
  switch (e.kind) {
    case 'mic_wake':
      return `“${s(e.text)}” · ${s(e.wake)}${e.rest ? ` · ${s(e.rest)}` : ''}`
    case 'mic_utterance':
      return `“${s(e.text)}” (${n(e.duration_ms)}ms)`
    case 'mic_ignored_unwoken':
      return `“${s(e.text)}”`
    case 'mic_segment_closed':
      return `${n(e.blocks)} blocks · ${n(e.duration_ms)}ms`
    case 'device_status':
      return `${s(e.device)} ${e.online ? 'online' : 'offline'} · ${s(e.reason)}`
    case 'assistant_state':
      return `${s(e.device)} · ${s(e.state)}`
    case 'tts_played':
      return `${n(e.synth_ms)}ms · ${s(e.play_id)}`
    case 'play_stage':
    case 'play_finished':
      return `${s(e.play_id)} ${s(e.stage)}${e.played_ms ? ` (${n(e.played_ms)}ms)` : ''}`
    case 'barge_in_stopped_plays':
      return `${s(e.reason)}: ${(e.plays as string[] | undefined)?.length ?? 0} plays`
    case 'barge_in_cancelled_task':
      return `${s(e.task)} (${s(e.reason)})`
    case 'turn_end':
      return `${s(e.status)} ${n(e.duration_ms)}ms`
    case 'volume_stage':
    case 'volume_set':
      return `${e.value ?? e.effective ?? '—'} · ${s(e.stage)}`
    case 'mute_state':
      return `${s(e.device)} ${e.muted ? '🔇' : '🔊'}`
    case 'button':
      return `${s(e.device)} ${s(e.action) || 'press'}`
    case 'button_stop':
      return `${s(e.device)} ⏹`
    case 'mic_report':
      return `${s(e.device)} ${e.active ? 'active' : 'inactive'}${e.detail ? ` (${s(e.detail)})` : ''}`
    case 'mode_set':
      return `${s(e.device)} duplex=${s(e.duplex_requested)}`
    case 'keyword':
      return `${s(e.device)} “${s(e.keyword)}”${e.fed_ms ? ` @${n(e.fed_ms)}ms` : ''}`
    case 'keyword_stop':
      return `${s(e.device)} ⏹ “${s(e.keyword)}” (${n(e.latency_ms)}ms)`
    case 'keyword_ignored':
      return `${s(e.device)} “${s(e.keyword)}” — ${s(e.reason)}`
    case 'kws_report':
      return `${s(e.device)} ${s(e.state)}${e.detail ? ` (${s(e.detail)})` : ''}`
    case 'barge_in_stop':
      return `${s(e.device)} ⏹🗣 (${n(e.latency_ms)}ms corr=${Number(e.corr).toFixed(2)} preroll=${n(e.preroll_ms)}ms)`
    case 'barge_in_ignored':
      return `${s(e.device)} 🗣 — ${s(e.reason)}`
    case 'barge_in_preroll_fed':
      return `${s(e.device)} pre-roll ${n(e.samples)} samples · VAD`
    case 'barge_in_preroll_skipped':
      return `${s(e.device)} pre-roll ${n(e.samples)} samples held back (${s(e.why)})`
    default: {
      const rest = { ...e, kind: undefined, ts_ms: undefined }
      return JSON.stringify(rest)
    }
  }
}
