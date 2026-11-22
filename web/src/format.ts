/**
 * Formatting helpers. Everything locale-aware goes through Intl, so the same
 * helpers render correctly in Chinese and English (RI-6).
 */
import { locale, t } from './i18n'

function tag(): string {
  return locale.value === 'zh' ? 'zh-Hans' : 'en'
}

export function formatTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '—'
  return new Intl.DateTimeFormat(tag(), { timeStyle: 'medium' }).format(new Date(ms))
}

export function formatDateTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '—'
  return new Intl.DateTimeFormat(tag(), { dateStyle: 'medium', timeStyle: 'medium' }).format(
    new Date(ms),
  )
}

export function formatRelative(ms: number | null | undefined, nowMs = Date.now()): string {
  if (ms == null || !Number.isFinite(ms)) return '—'
  const diff = nowMs / 1000 - ms / 1000
  const abs = Math.abs(diff)
  const rtf = new Intl.RelativeTimeFormat(tag(), { numeric: 'auto', style: 'short' })
  if (abs < 60) return rtf.format(-Math.round(diff), 'second')
  if (abs < 3600) return rtf.format(-Math.round(diff / 60), 'minute')
  if (abs < 86400) return rtf.format(-Math.round(diff / 3600), 'hour')
  return rtf.format(-Math.round(diff / 86400), 'day')
}

/** Durations read as `850 ms`, `1.2 s`, `2 min 05 s`, `1 h 04 min`.
 *
 * Unit words come from the dictionary (audit R2-28): the old version printed
 * `min`/`h` inside Chinese copy while the same page printed `秒` elsewhere. */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '—'
  const value = Math.max(0, Math.round(ms))
  if (value < 1000) return `${value} ${t('unitMs')}`
  if (value < 60_000) return `${(value / 1000).toFixed(value < 10_000 ? 2 : 1)} ${t('unitS')}`
  const totalSec = Math.round(value / 1000)
  const min = Math.floor(totalSec / 60)
  const sec = totalSec % 60
  if (min < 60) return `${min} ${t('unitMin')} ${String(sec).padStart(2, '0')} ${t('unitSec')}`
  const hour = Math.floor(min / 60)
  return `${hour} ${t('unitHour')} ${String(min % 60).padStart(2, '0')} ${t('unitMin')}`
}

/** Numbers of characters (回答长度上限). English says "chars", Chinese 字. */
export function formatChars(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return '—'
  return `${formatNumber(n)} ${t('unitChars')}`
}

export function formatNumber(value: number | null | undefined, digits = 0): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return new Intl.NumberFormat(tag(), {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  }).format(value)
}

export function formatPercent(value: number | null | undefined, digits = 0): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return `${value.toFixed(digits)}%`
}

export function formatDb(value: number | null | undefined, digits = 1): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return `${value.toFixed(digits)} dB`
}

export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes)) return '—'
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(2)} MB`
}

export function formatValue(value: unknown): string {
  if (value === null || value === undefined || value === '') return t('none')
  if (typeof value === 'boolean') return value ? t('yes') : t('no')
  if (typeof value === 'number') return String(value)
  if (typeof value === 'string') return value
  try {
    return JSON.stringify(value)
  } catch {
    return String(value)
  }
}

export function errorDetail(err: unknown): string {
  if (err && typeof err === 'object' && 'detail' in err && typeof (err as { detail: unknown }).detail === 'string') {
    return (err as { detail: string }).detail
  }
  if (err instanceof Error) return err.message
  return String(err)
}

export function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value))
}
