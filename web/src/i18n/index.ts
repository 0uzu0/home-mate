/**
 * Console copy, Simplified Chinese and English, complete in both.
 *
 * `Key` is derived from the Chinese dictionary (assembled from `zh/`), and the
 * English dictionary is typed as `Record<Key, string>` — a missing or extra key
 * is a compile error, which is what keeps the two languages aligned (RI-6).
 *
 * The dictionaries themselves live one file per page under `zh/` and `en/`;
 * this module only holds the language switch and `t()`.
 *
 * Values may contain `{name}` placeholders; `t()` fills them from `params`.
 */
import { computed, ref } from 'vue'
import { zh, type Key } from './zh'
import en from './en'

export type { Key }

export type Locale = 'zh' | 'en'

const STORAGE_KEY = 'hm.locale'

const DICTS: Record<Locale, Record<Key, string>> = { zh, en }

function readLocale(): Locale {
  try {
    return localStorage.getItem(STORAGE_KEY) === 'en' ? 'en' : 'zh'
  } catch {
    return 'zh'
  }
}

export const locale = ref<Locale>(readLocale())

export type Params = Record<string, string | number>

export function t(key: Key, params?: Params): string {
  const raw = DICTS[locale.value][key] ?? String(key)
  if (!params) return raw
  return raw.replace(/\{(\w+)\}/g, (match, name: string) =>
    name in params ? String(params[name]) : match,
  )
}

/** Non-reactive `t` for module-level helpers that only need the current value. */
export const tr = t

export function setLocale(next: Locale): void {
  locale.value = next
  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    /* private mode */
  }
  initLocale()
}

export function initLocale(): void {
  document.documentElement.lang = locale.value === 'zh' ? 'zh-Hans' : 'en'
  document.title = `${t('brand')} · ${t('brandSub')}`
}

export const localeLabel = computed(() => (locale.value === 'zh' ? 'en' : 'zh'))

