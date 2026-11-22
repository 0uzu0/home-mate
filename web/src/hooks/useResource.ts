/**
 * Minimal async resource: latest request wins, every attempt has a timeout,
 * transient failures retry with a fresh attempt.
 *
 * The loader receives an AbortSignal. When a newer load starts (or the scope
 * unmounts) the in-flight attempt is aborted, so a slow poll can never
 * overwrite fresher data and navigating away stops network work.
 */
import { computed, onScopeDispose, ref, shallowRef, type ComputedRef, type Ref, type ShallowRef } from 'vue'
import { ApiError, isAbort } from '../api'

export interface ResourceOptions {
  /** Per-attempt timeout (ms). */
  timeoutMs?: number
  /** Extra attempts after the first one (transient errors only). */
  retries?: number
  retryDelayMs?: number
}

export interface Resource<T> {
  data: ShallowRef<T | null>
  error: ShallowRef<unknown>
  loading: Ref<boolean>
  updatedAt: Ref<number | null>
  /**
   * Data is showing AND the latest refresh failed. The card must say so: a
   * stale number presented as current is the console lying (audit R1-10).
   */
  stale: ComputedRef<boolean>
  load: () => Promise<void>
  reset: () => void
}

function transient(err: unknown): boolean {
  return (
    err instanceof ApiError &&
    (err.code === 'network' ||
      err.code === 'timeout' ||
      err.code === 'bad_response' ||
      err.code === 'server_error')
  )
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms))
}

export function useResource<T>(
  loader: (signal: AbortSignal) => Promise<T>,
  options: ResourceOptions = {},
): Resource<T> {
  const timeoutMs = options.timeoutMs ?? 8000
  const retries = options.retries ?? 1
  const retryDelayMs = options.retryDelayMs ?? 350

  const data = shallowRef<T | null>(null)
  const error = shallowRef<unknown>(null)
  const loading = ref(false)
  const updatedAt = ref<number | null>(null)
  const stale = computed(() => data.value !== null && error.value !== null)

  let sequence = 0
  let inFlight: AbortController | null = null

  async function attemptOnce(): Promise<T> {
    const controller = new AbortController()
    inFlight = controller
    let timedOut = false
    const timer = window.setTimeout(() => {
      timedOut = true
      controller.abort()
    }, timeoutMs)
    try {
      return await loader(controller.signal)
    } catch (err) {
      if (timedOut) throw new ApiError('timeout', `timeout after ${timeoutMs}ms`)
      throw err
    } finally {
      window.clearTimeout(timer)
    }
  }

  async function load(): Promise<void> {
    const mine = ++sequence
    inFlight?.abort()
    loading.value = true
    error.value = null
    try {
      for (let attemptIndex = 0; ; attemptIndex += 1) {
        try {
          const value = await attemptOnce()
          if (mine !== sequence) return
          data.value = value
          updatedAt.value = Date.now()
          return
        } catch (err) {
          // Superseded by a newer load or unmounted: stay silent.
          if (mine !== sequence || isAbort(err)) return
          if (attemptIndex < retries && transient(err)) {
            await sleep(retryDelayMs)
            if (mine !== sequence) return
            continue
          }
          error.value = err
          return
        }
      }
    } finally {
      if (mine === sequence) loading.value = false
    }
  }

  function reset(): void {
    sequence += 1
    inFlight?.abort()
    inFlight = null
    data.value = null
    error.value = null
    loading.value = false
    updatedAt.value = null
  }

  onScopeDispose(() => {
    sequence += 1
    inFlight?.abort()
  })

  return { data, error, loading, updatedAt, stale, load, reset }
}
