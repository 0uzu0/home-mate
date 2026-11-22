<script setup lang="ts">
/** Renders the global toast queue as a polite live region. */
import { toasts, toast } from '../hooks/useToast'
import AppIcon from './AppIcon.vue'
</script>

<template>
  <div class="toasts" role="status" aria-live="polite">
    <div
      v-for="item in toasts"
      :key="item.id"
      class="toast"
      :class="`toast--${item.tone}`"
    >
      <span class="toast__text">{{ item.text }}</span>
      <button type="button" class="toast__close" aria-label="×" @click="toast.dismiss(item.id)">
        <AppIcon name="close" :size="13" />
      </button>
    </div>
  </div>
</template>

<style scoped>
.toasts {
  position: fixed;
  right: var(--sp-4);
  bottom: var(--sp-4);
  z-index: 40;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  max-width: min(26rem, calc(100vw - 2rem));
  pointer-events: none;
}

.toast {
  pointer-events: auto;
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--sp-3);
  padding: var(--sp-2) var(--sp-3);
  border-radius: var(--r-sm);
  border: 1px solid var(--line);
  background: var(--surface);
  box-shadow: var(--shadow-pop);
  font-size: var(--fs-sm);
  color: var(--ink);
}

.toast--ok {
  border-color: var(--ok-line);
  background: var(--ok-soft);
  color: var(--ok);
}

.toast--error {
  border-color: var(--danger-line);
  background: var(--danger-soft);
  color: var(--danger);
}

.toast--info {
  border-color: var(--info-line);
  background: var(--info-soft);
  color: var(--info);
}

.toast__text {
  word-break: break-word;
}

.toast__close {
  border: 0;
  background: none;
  color: inherit;
  cursor: pointer;
  font-size: var(--fs-md);
  line-height: 1;
  padding: 0;
  opacity: 0.7;
}

.toast__close:hover {
  opacity: 1;
}
</style>
