/**
 * Appearance: auto / light / dark, remembered per browser.
 *
 * The inline bootstrap in index.html applies the same value before first
 * paint, so the console never flashes the wrong theme. This module owns the
 * switch after that; `resolvedTheme` mirrors what the document actually has.
 */
import { computed, ref } from 'vue'

export type ThemePref = 'auto' | 'light' | 'dark'
export type ResolvedTheme = 'light' | 'dark'

const STORAGE_KEY = 'hm.theme'

function readPreference(): ThemePref {
  try {
    const saved = localStorage.getItem(STORAGE_KEY)
    // The short-lived gold skin is gone; a browser that still remembers it
    // falls back to auto instead of being stuck without a theme.
    if (saved === 'brand') {
      try {
        localStorage.setItem(STORAGE_KEY, 'auto')
      } catch {
        /* private mode */
      }
      return 'auto'
    }
    if (saved === 'light' || saved === 'dark' || saved === 'auto') return saved
  } catch {
    /* private mode */
  }
  return 'auto'
}

export const themePref = ref<ThemePref>(readPreference())
export const resolvedTheme = ref<ResolvedTheme>('light')

const query =
  typeof window !== 'undefined' && window.matchMedia
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null

function resolve(): ResolvedTheme {
  if (themePref.value === 'light' || themePref.value === 'dark') return themePref.value
  return query?.matches ? 'dark' : 'light'
}

export function applyTheme(): void {
  resolvedTheme.value = resolve()
  document.documentElement.dataset.theme = resolvedTheme.value
  // The browser chrome colour comes from the tokens themselves, so there is
  // no second copy of the palette in this file.
  const meta = document.querySelector('meta[name="theme-color"]')
  if (meta) {
    const bg = getComputedStyle(document.documentElement).getPropertyValue('--bg').trim()
    if (bg) meta.setAttribute('content', bg)
  }
}

export function setThemePref(next: ThemePref): void {
  themePref.value = next
  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    /* private mode */
  }
  applyTheme()
}

export function initTheme(): void {
  applyTheme()
  query?.addEventListener('change', () => {
    if (themePref.value === 'auto') applyTheme()
  })
}

/** auto → light → dark → auto; the header button cycles this. */
export function cycleTheme(): void {
  setThemePref(themePref.value === 'auto' ? 'light' : themePref.value === 'light' ? 'dark' : 'auto')
}

export const themeLabelKey = computed(() => {
  switch (themePref.value) {
    case 'light':
      return 'themeLight' as const
    case 'dark':
      return 'themeDark' as const
    default:
      return 'themeAuto' as const
  }
})
