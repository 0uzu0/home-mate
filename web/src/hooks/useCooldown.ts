/**
 * Server-advertised cooldown (429). `Retry-After` / `details.retry_after_s`
 * is the source; the countdown only disables a submit button, it never
 * retries on its own.
 */
import { computed, onBeforeUnmount, ref } from 'vue'
import { ApiError } from '../api'

export function useCooldown() {
  const seconds = ref(0)
  let timer: number | null = null

  function clearTimer(): void {
    if (timer !== null) {
      window.clearInterval(timer)
      timer = null
    }
  }

  function start(value: number): void {
    clearTimer()
    seconds.value = Math.max(0, Math.ceil(value))
    if (seconds.value <= 0) return
    timer = window.setInterval(() => {
      seconds.value -= 1
      if (seconds.value <= 0) clearTimer()
    }, 1000)
  }

  function clear(): void {
    clearTimer()
    seconds.value = 0
  }

  /** Feed an API error through here; returns true when it was a 429. */
  function fromError(err: unknown): boolean {
    if (!(err instanceof ApiError) || err.code !== 'cooldown') return false
    start(30)
    return true
  }

  onBeforeUnmount(clearTimer)

  return {
    seconds,
    active: computed(() => seconds.value > 0),
    start,
    clear,
    fromError,
  }
}
