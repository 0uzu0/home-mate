<script setup lang="ts">
/**
 * Household access credential (M9): state, rotation, and forgetting this
 * browser.
 *
 * The token itself is never stored by the core in plaintext and is never
 * displayed twice — rotation shows the replacement exactly once, and this
 * browser adopts it immediately so the page does not lock itself out.
 */
import { computed, ref } from 'vue'
import { getAuthStatus, provisionAuth, rotateAuth, setLanGatewayExempt } from '../../api'
import { authToken, clearAuthToken, setAuthToken } from '../../auth'
import { formatDateTime, formatRelative } from '../../format'
import { t } from '../../i18n'
import { usePoll } from '../../hooks/usePoll'
import { useResource } from '../../hooks/useResource'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import AppIcon from '../AppIcon.vue'
import ConfirmDialog from '../ConfirmDialog.vue'
import Toggle from '../Toggle.vue'

const auth = useResource((signal) => getAuthStatus({ signal }), { timeoutMs: 5000, retries: 1 })
usePoll(() => auth.load(), 10000)

const { busy, message, tone, run } = useSave()
const confirming = ref(false)
const fresh = ref<string | null>(null)
const copied = ref(false)
const toggleKey = ref(0)

const entry = computed(() => auth.data.value?.tokens?.[0] ?? null)
const paired = computed(() => auth.data.value?.configured === true)
const coreId = computed(() => auth.data.value?.core_id ?? '')
const lanExempt = computed(() => auth.data.value?.lan_gateway_exempt === true)

async function toggleLanExempt(next: boolean): Promise<void> {
  try {
    await run(async () => {
      const resp = await setLanGatewayExempt(next, { timeoutMs: 8000 })
      await auth.load()
      return resp.lan_gateway_exempt ? t('authLanExemptEnabled') : t('authLanExemptDisabled')
    })
  } finally {
    // Re-sync DOM checkbox with authoritative server value if the save failed
    toggleKey.value += 1
  }
}

async function copy(): Promise<void> {
  if (!fresh.value) return
  try {
    await navigator.clipboard.writeText(fresh.value)
    copied.value = true
  } catch {
    copied.value = false
  }
}

async function provision(): Promise<void> {
  await run(async () => {
    const resp = await provisionAuth({ timeoutMs: 8000 })
    setAuthToken(resp.token)
    fresh.value = resp.token
    copied.value = false
    await auth.load()
    return t('authIssuedTitle')
  })
}

async function rotate(): Promise<void> {
  confirming.value = false
  await run(async () => {
    const resp = await rotateAuth({ timeoutMs: 8000 })
    setAuthToken(resp.token)
    fresh.value = resp.token
    copied.value = false
    await auth.load()
    return t('authRotateDone')
  })
}

async function forget(): Promise<void> {
  await run(async () => {
    clearAuthToken()
    // The next request 401s and the shell hands over to the pairing card.
    await auth.load().catch(() => undefined)
    return t('authClearDone')
  })
}
</script>

<template>
  <Panel icon="device" :title="t('authCardTitle')" :hint="t('authCardHint')">
    <div class="auth">
      <div class="auth__row">
        <Pill :tone="paired ? 'ok' : 'warn'" size="sm">
          <AppIcon :name="paired ? 'check' : 'alert'" :size="12" />
          {{ paired ? t('authStatePaired') : t('authStateUnpaired') }}
        </Pill>
        <span v-if="entry" class="auth__meta">
          {{ t('authCreatedAt') }} {{ formatDateTime(entry.created_ms) }}
        </span>
        <span v-if="entry" class="auth__meta">
          {{ t('authLastUsed') }}
          {{ entry.last_used_ms ? formatRelative(entry.last_used_ms) : t('authNeverUsed') }}
        </span>
        <span class="auth__meta mono">{{ authToken ? 'hm1_••••' : '—' }}</span>
        <span v-if="coreId" class="auth__meta mono">{{ t('authCoreId') }}: {{ coreId }}</span>
      </div>

      <div class="auth__exempt">
        <Toggle
          :key="toggleKey"
          :model-value="lanExempt"
          :disabled="busy"
          :label="t('authLanExemptTitle')"
          :hint="t('authLanExemptHint')"
          @update:model-value="toggleLanExempt"
        />
      </div>

      <p v-if="auth.data.value?.corrupt" class="auth__error">{{ t('authCorruptBody') }}</p>

      <div v-if="fresh" class="auth__token">
        <code class="auth__value">{{ fresh }}</code>
        <div class="auth__actions">
          <button type="button" class="btn" @click="copy">
            <AppIcon name="check" :size="13" /> {{ copied ? t('authCopied') : t('authCopy') }}
          </button>
          <span class="auth__meta">{{ t('authShowOnce') }}</span>
        </div>
      </div>

      <div class="auth__actions">
        <button v-if="!paired" type="button" class="btn btn--primary" :disabled="busy" @click="provision">
          <AppIcon name="check" :size="13" /> {{ t('authGenerate') }}
        </button>
        <button
          v-else
          type="button"
          class="btn btn--danger"
          :disabled="busy"
          @click="confirming = true"
        >
          <AppIcon name="reload" :size="13" /> {{ t('authRotate') }}
        </button>
        <button type="button" class="btn" :disabled="busy" @click="forget">
          {{ t('authClearLocal') }}
        </button>
        <span v-if="message" class="auth__msg" :class="`auth__msg--${tone}`">{{ message }}</span>
      </div>
    </div>

    <ConfirmDialog
      v-if="confirming"
      :title="t('authRotateConfirmTitle')"
      :message="t('authRotateConfirmBody')"
      :confirm-label="t('authRotateConfirm')"
      :cancel-label="t('cancel')"
      :busy="busy"
      :busy-label="t('authRotate')"
      tone="danger"
      @confirm="rotate"
      @cancel="confirming = false"
    />
  </Panel>
</template>

<style scoped>
.auth {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.auth__row {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.auth__meta {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.auth__exempt {
  padding-top: var(--sp-2);
  border-top: 1px solid var(--line);
}

.auth__token {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  background: var(--surface-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  padding: var(--sp-3);
}

.auth__value {
  word-break: break-all;
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  user-select: all;
}

.auth__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.auth__error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.auth__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.auth__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.btn {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.26rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:hover:not(:disabled) {
  background: var(--surface-3);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.btn--primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.btn--danger {
  border-color: var(--danger);
  color: var(--danger);
  background: transparent;
}

.btn--danger:hover:not(:disabled) {
  background: var(--danger);
  color: var(--surface);
}
</style>
