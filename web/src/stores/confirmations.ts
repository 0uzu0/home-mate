import { computed, reactive, ref, watch } from 'vue'
import { ApiError, getConfirmation, listConfirmations, transitionConfirmation,
  type ConfirmationAction, type ConfirmationTransition } from '../api'
import { authToken } from '../auth'

// Server records are the only source of action identity, authority and status.
// Nothing is persisted in browser storage, including redacted action previews.
export const confirmationActions = reactive(new Map<string, ConfirmationAction>())
export const confirmationErrors = reactive(new Map<string, unknown>())
export const confirmationBusy = reactive(new Set<string>())
export const confirmationIndexError = ref<unknown>(null)
export const confirmationLoading = ref(false)
export const confirmationCursor = ref<string | null>(null)
export const confirmationLoaded = ref(false)
export const pendingConfirmations = computed(() => [...confirmationActions.values()]
  .filter(a => (a.state === 'pending' || a.state === 'approved') && !confirmationUnavailable(confirmationErrors.get(a.id))))
export const sortedConfirmations = computed(() => [...confirmationActions.values()]
  .sort((a, b) => Number(b.state === 'pending' || b.state === 'approved')
    - Number(a.state === 'pending' || a.state === 'approved') || b.created_ms - a.created_ms))
let generation = 0
let mutationEpoch = 0
let controller = new AbortController()
let refreshInFlight: Promise<void> | null = null
const reads = new Map<string, Promise<void>>()

watch(authToken, () => {
  generation += 1
  controller.abort(); controller = new AbortController()
  confirmationActions.clear(); confirmationErrors.clear(); confirmationBusy.clear()
  confirmationIndexError.value = null; confirmationLoaded.value = false
  confirmationCursor.value = null; confirmationLoading.value = false
  refreshInFlight = null; reads.clear()
}, { flush: 'sync' })

export function receiveConfirmation(action: ConfirmationAction): void {
  if (!authToken.value || !action?.id || !Number.isSafeInteger(action.revision)) return
  const existing = confirmationActions.get(action.id)
  if (!existing || action.revision >= existing.revision) confirmationActions.set(action.id, action)
}

export function refreshConfirmation(id: string): Promise<void> {
  const existing = reads.get(id)
  if (existing) return existing
  if (!authToken.value || confirmationBusy.has(id)) return Promise.resolve()
  const current = generation
  const mutationStart = mutationEpoch
  const work = (async () => {
    try {
      const action = await getConfirmation(id, { signal: controller.signal })
      if (current !== generation) return
      receiveConfirmation(action)
      if (mutationStart === mutationEpoch) confirmationErrors.delete(id)
    } catch (err) {
      if (current === generation) {
        if (err instanceof ApiError && err.status === 404) confirmationActions.delete(id)
        confirmationErrors.set(id, err)
      }
    } finally { if (current === generation) reads.delete(id) }
  })()
  reads.set(id, work)
  return work
}

export function refreshConfirmations(more = false): Promise<void> {
  if (refreshInFlight) return refreshInFlight
  if (!authToken.value) return Promise.resolve()
  const current = generation
  const mutationStart = mutationEpoch
  confirmationLoading.value = true
  const work = (async () => {
    try {
      // Fetch live actions separately so recent history cannot bury a voice
      // request. Pagination is bounded and explicitly continued by the user.
      const [live, recent] = await Promise.all([
        listConfirmations(['pending', 'approved'], more ? confirmationCursor.value : null, { signal: controller.signal }),
        listConfirmations(['consumed', 'rejected', 'expired', 'cancelled', 'stale'], null, { signal: controller.signal }),
      ])
      if (current !== generation) return
      for (const action of [...live.actions, ...recent.actions]) {
        receiveConfirmation(action)
        if (!confirmationBusy.has(action.id) && mutationStart === mutationEpoch) confirmationErrors.delete(action.id)
      }
      if (more || !confirmationLoaded.value) confirmationCursor.value = live.next_cursor
      confirmationIndexError.value = null; confirmationLoaded.value = true
      // A cancelled/expired request may no longer be on either first page.
      const returned = new Set([...live.actions, ...recent.actions].map(a => a.id))
      // Keep only the bounded current history page. Removed/reset payloads
      // cannot linger indefinitely in an already-open console.
      for (const action of confirmationActions.values()) {
        if (mutationStart === mutationEpoch && !['pending', 'approved'].includes(action.state) && !returned.has(action.id)
            && !confirmationBusy.has(action.id)) confirmationActions.delete(action.id)
      }
      await Promise.all(pendingConfirmations.value.filter(a => !returned.has(a.id))
        .map(a => refreshConfirmation(a.id)))
    } catch (err) {
      if (current === generation) {
        confirmationIndexError.value = err
        for (const id of confirmationActions.keys()) confirmationErrors.set(id, err)
      }
    } finally {
      if (current === generation) { confirmationLoading.value = false; refreshInFlight = null }
    }
  })()
  refreshInFlight = work
  return work
}

export async function changeConfirmation(id: string, transition: ConfirmationTransition): Promise<void> {
  const action = confirmationActions.get(id)
  if (!authToken.value || !action || confirmationBusy.has(id) || confirmationErrors.has(id)) return
  if (transition === 'execute' ? action.state !== 'approved' : !['pending', 'approved'].includes(action.state)) return
  if (transition === 'approve' && action.state !== 'pending') return
  if (Date.now() >= action.expires_ms) { await refreshConfirmation(id); return }
  const current = generation
  mutationEpoch += 1
  confirmationBusy.add(id)
  try {
    const updated = await transitionConfirmation(action, transition, { signal: controller.signal })
    if (current !== generation) return
    receiveConfirmation(updated); confirmationErrors.delete(id)
  } catch (err) {
    if (current !== generation) return
    // A lost POST response is never a reason to repeat the mutation. Preserve
    // the preview, disable transitions, and reconcile using server GET only.
    confirmationErrors.set(id, err)
  } finally {
    if (current === generation) confirmationBusy.delete(id)
  }
}

export function confirmationUnavailable(error: unknown): boolean {
  return error instanceof ApiError && [401, 404].includes(error.status)
}
