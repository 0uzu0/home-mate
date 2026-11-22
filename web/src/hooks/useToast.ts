/**
 * One global toast queue. Cards call `toast.ok(...)` / `toast.error(...)`
 * for outcomes that are not attached to a field; the shell renders them in a
 * live region so screen readers hear saves and failures too.
 */
import { readonly, ref } from 'vue'

export type ToastTone = 'ok' | 'error' | 'info'

export interface ToastItem {
  id: number
  tone: ToastTone
  text: string
}

const items = ref<ToastItem[]>([])
let nextId = 1

function push(tone: ToastTone, text: string, ttlMs = 4200): void {
  if (!text) return
  const id = nextId++
  items.value = [...items.value, { id, tone, text }]
  window.setTimeout(() => dismiss(id), ttlMs)
}

function dismiss(id: number): void {
  items.value = items.value.filter((item) => item.id !== id)
}

export const toasts = readonly(items)

export const toast = {
  ok: (text: string) => push('ok', text),
  error: (text: string) => push('error', text, 6000),
  info: (text: string) => push('info', text),
  dismiss,
}
