<script setup lang="ts">
/**
 * One error, one shape: the server's detail (when it gave one), a stable code
 * for the log, and a retry that actually re-runs the read.
 *
 * `stale` is the quieter variant used when the card still has older data on
 * screen: the read failed, the numbers below are from `at`. Without it a
 * failed refresh looked like fresh data (audit R1-10/R2-27).
 */
import { computed } from 'vue'
import { ApiError } from '../api'
import { formatTime } from '../format'
import { t } from '../i18n'
import AppIcon from './AppIcon.vue'

const props = withDefaults(
  defineProps<{
    error: unknown
    retryLabel?: string
    compact?: boolean
    stale?: boolean
    /** When the data on screen was read (ms), for the stale line. */
    at?: number | null
  }>(),
  { retryLabel: '', compact: false, stale: false, at: null },
)

const emit = defineEmits<{ retry: [] }>()

const detail = computed(() => {
  if (props.error instanceof ApiError) {
    if (props.error.code === 'network') return t('errNetwork')
    if (props.error.code === 'timeout') return t('errTimeout')
    return props.error.detail || props.error.code
  }
  if (props.error instanceof Error) return props.error.message
  return t('errUnknown')
})

const code = computed(() => (props.error instanceof ApiError ? props.error.code : 'unknown'))
const staleLine = computed(() =>
  props.stale
    ? props.at
      ? t('staleDataAt', { time: formatTime(props.at) })
      : t('staleData')
    : '',
)
</script>

<template>
  <div class="error" :class="{ 'error--compact': compact, 'error--stale': stale }" role="alert">
    <AppIcon name="alert" :size="18" class="error__icon" />
    <div class="error__body">
      <p v-if="staleLine" class="error__stale">{{ staleLine }}</p>
      <p class="error__detail">{{ detail }}</p>
      <p class="error__code mono">{{ code }}</p>
    </div>
    <button type="button" class="error__retry" @click="emit('retry')">
      {{ retryLabel || t('retry') }}
    </button>
  </div>
</template>

<style scoped>
.error {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-4);
  border: 1px solid var(--danger-line);
  border-radius: var(--r-sm);
  background: var(--danger-soft);
  color: var(--danger);
}

.error--compact {
  padding: var(--sp-2) var(--sp-3);
}

/* Stale is a caution, not a failure: the data below still means something. */
.error--stale {
  border-color: var(--warn-line, var(--line-strong));
  background: var(--warn-soft, var(--surface-2));
  color: var(--ink);
}

.error__stale {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--warn, var(--ink));
}

.error__icon {
  flex: none;
}

.error__body {
  min-width: 0;
}

.error__detail {
  font-size: var(--fs-base);
  word-break: break-word;
}

.error__code {
  font-size: var(--fs-xs);
  opacity: 0.75;
}

.error__retry {
  flex: none;
  border: 1px solid currentColor;
  background: transparent;
  color: inherit;
  border-radius: var(--r-sm);
  padding: 0.2rem var(--sp-3);
  cursor: pointer;
}

.error__retry:hover {
  background: var(--danger);
  color: var(--surface);
}
</style>
