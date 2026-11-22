<script setup lang="ts">
/**
 * Blocking second confirmation for destructive or overwriting actions.
 * Escape and Cancel never submit; the confirm button is the only way forward.
 */
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue'

const props = withDefaults(
  defineProps<{
    title: string
    message: string
    confirmLabel: string
    cancelLabel: string
    tone?: 'default' | 'danger'
    busy?: boolean
    busyLabel?: string
  }>(),
  { tone: 'default', busy: false, busyLabel: '' },
)

const emit = defineEmits<{ confirm: []; cancel: [] }>()

const dialog = ref<HTMLElement | null>(null)
let previous: HTMLElement | null = null

function cancel(): void {
  if (props.busy) return
  emit('cancel')
}

const FOCUSABLE =
  'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])'

function focusables(): HTMLElement[] {
  const root = dialog.value
  if (!root) return []
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) => el.getClientRects().length > 0,
  )
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key === 'Escape') {
    event.preventDefault()
    cancel()
    return
  }
  if (event.key !== 'Tab') return
  const items = focusables()
  if (!items.length) {
    event.preventDefault()
    dialog.value?.focus()
    return
  }
  const first = items[0]
  const last = items[items.length - 1]
  const active = document.activeElement
  const inside = active instanceof HTMLElement && dialog.value?.contains(active)
  if (event.shiftKey) {
    if (!inside || active === first) {
      event.preventDefault()
      last.focus()
    }
    return
  }
  if (!inside || active === last) {
    event.preventDefault()
    first.focus()
  }
}

onMounted(() => {
  previous = document.activeElement instanceof HTMLElement ? document.activeElement : null
  document.addEventListener('keydown', onKeydown)
  void nextTick(() => {
    const cancelButton = dialog.value?.querySelector<HTMLElement>('[data-dialog-cancel]')
    ;(cancelButton ?? dialog.value)?.focus()
  })
})

onBeforeUnmount(() => {
  document.removeEventListener('keydown', onKeydown)
  previous?.focus()
})
</script>

<template>
  <Teleport to="body">
    <div class="overlay" @click.self="cancel">
      <div
        ref="dialog"
        class="dialog"
        role="alertdialog"
        aria-modal="true"
        :aria-label="title"
        tabindex="-1"
      >
        <h2 class="dialog__title">{{ title }}</h2>
        <p class="dialog__message">{{ message }}</p>
        <div class="dialog__actions">
          <button type="button" class="btn" data-dialog-cancel @click="cancel">
            {{ cancelLabel }}
          </button>
          <button
            type="button"
            class="btn"
            :class="tone === 'danger' ? 'btn--danger' : 'btn--primary'"
            :disabled="busy"
            @click="emit('confirm')"
          >
            {{ busy ? busyLabel || '…' : confirmLabel }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.overlay {
  position: fixed;
  inset: 0;
  z-index: 50;
  background: var(--overlay);
  display: flex;
  align-items: center;
  justify-content: center;
  padding: var(--sp-4);
}

.dialog {
  width: min(30rem, 100%);
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--r-lg);
  box-shadow: var(--shadow-pop);
  padding: var(--sp-5);
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.dialog__title {
  font-size: var(--fs-lg);
}

.dialog__message {
  color: var(--ink-2);
  font-size: var(--fs-base);
}

.dialog__actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--sp-2);
  margin-top: var(--sp-2);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.3rem var(--sp-4);
  cursor: pointer;
}

.btn:hover {
  background: var(--surface-3);
}

.btn--primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.btn--primary:hover {
  background: var(--accent-hover);
}

.btn--danger {
  background: var(--danger);
  border-color: var(--danger);
  color: var(--surface);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
