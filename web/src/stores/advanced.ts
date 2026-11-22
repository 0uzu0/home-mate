import { ref } from 'vue'

const STORAGE_KEY = 'hm-console-advanced'

function loadAdvanced(): boolean {
  try {
    return localStorage.getItem(STORAGE_KEY) === 'true'
  } catch {
    return false
  }
}

/**
 * Advanced mode toggle (#14): false by default for family use.
 * When false, Diagnostics, Releases, direct firmware upload, secret-included
 * export, and Factory Reset are hidden from main nav and daily pages.
 */
export const advancedMode = ref<boolean>(loadAdvanced())

export function setAdvancedMode(enabled: boolean): void {
  advancedMode.value = enabled
  try {
    localStorage.setItem(STORAGE_KEY, enabled ? 'true' : 'false')
  } catch {
    /* Storage may be disabled. */
  }
}

export function toggleAdvancedMode(): void {
  setAdvancedMode(!advancedMode.value)
}
