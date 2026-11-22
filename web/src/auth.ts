/**
 * The household access token kept by THIS browser (M9).
 *
 * One token covers the household plane (the user's model choice: a single
 * family token, not per-client pairing yet). It lives in localStorage because
 * a page reload must not force a re-pair; it is never rendered anywhere except
 * the one screen that issued it.
 *
 * This module stays dependency-free on purpose: `api.ts` imports it, so it
 * must not import `api.ts` back.
 */
import { ref } from 'vue'

const STORAGE_KEY = 'hm.api.token'

function read(): string | null {
  try {
    const value = window.localStorage.getItem(STORAGE_KEY)
    return value && value.startsWith('hm1_') ? value : null
  } catch {
    // Private mode / storage disabled: the session still works, it just
    // needs a re-pair after a reload.
    return null
  }
}

export const authToken = ref<string | null>(read())

/** Set by the API client on any 401; the shell hands over to the gate. */
export const pairingRequired = ref(false)

export function setAuthToken(token: string): void {
  const value = token.trim()
  try {
    window.localStorage.setItem(STORAGE_KEY, value)
  } catch {
    /* see read() */
  }
  authToken.value = value
  pairingRequired.value = false
}

export function clearAuthToken(): void {
  try {
    window.localStorage.removeItem(STORAGE_KEY)
  } catch {
    /* see read() */
  }
  authToken.value = null
}

/** A 401 was observed: the credential is dead (rotated) or missing. */
export function markUnauthorized(): void {
  pairingRequired.value = true
}
