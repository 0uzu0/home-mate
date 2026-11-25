<script setup lang="ts">
/**
 * The household pairing gate (M9).
 *
 * The shell renders only after a token is in hand, so no page ever polls
 * unauthenticated. Four states:
 *
 * - `checking`: asking the open `/api/v0/auth` endpoint whether this device
 *   was ever paired (a fresh device must not look like a broken console);
 * - `setup`: never paired — generate the household token, show it once;
 * - `pair`: paired, but this browser has no valid token (new browser, or the
 *   token was rotated);
 * - `corrupt`: the core's token store is unreadable and the API fails closed;
 *   recovery is a root/serial action, not something a console may do.
 */
import { computed, ref } from 'vue'
import { getAuthStatus, getStatus, provisionAuth, type ApiError } from '../api'
import { setAuthToken, clearAuthToken } from '../auth'
import { locale, setLocale, t } from '../i18n'
import { cycleTheme, themePref } from '../theme'
import AppIcon from './AppIcon.vue'
import BrandLogo from './BrandLogo.vue'
import TextInput from './TextInput.vue'

const props = defineProps<{ state: 'checking' | 'setup' | 'pair' | 'corrupt' }>()
const emit = defineEmits<{ ready: []; retry: [] }>()

const busy = ref(false)
const error = ref('')
const issued = ref<string | null>(null)
const acknowledged = ref(false)
const copied = ref(false)
const candidate = ref('')

const themeLabel = computed(() => {
  switch (themePref.value) {
    case 'light':
      return t('themeLight')
    case 'dark':
      return t('themeDark')
    default:
      return t('themeAuto')
  }
})

function detail(err: unknown): string {
  const e = err as ApiError
  return e?.detail || (e?.message ?? String(err))
}

async function generate(): Promise<void> {
  busy.value = true
  error.value = ''
  try {
    const resp = await provisionAuth({ timeoutMs: 8000 })
    issued.value = resp.token
    copied.value = false
    acknowledged.value = false
  } catch (err) {
    // 409 = another tab/client paired first; re-check rather than overwrite.
    error.value = detail(err)
    if ((err as ApiError)?.status === 409) emit('retry')
  } finally {
    busy.value = false
  }
}

async function copy(): Promise<void> {
  if (!issued.value) return
  try {
    await navigator.clipboard.writeText(issued.value)
    copied.value = true
  } catch {
    // Clipboard can be denied outside a secure context; the field is
    // selectable, which is why it is rendered as text and not a password box.
    copied.value = false
  }
}

function enter(): void {
  if (!issued.value || !acknowledged.value) return
  setAuthToken(issued.value)
  emit('ready')
}

function sanitizeToken(raw: string): string {
  let value = raw.replace(/[\r\n]+/g, '')
  let prev = ''
  while (value !== prev) {
    prev = value
    value = value
      .trim()
      .replace(/^["'`]+|["'`]+$/g, '')
      .replace(/^\s*bearer(?:\s+|$)/i, '')
      .trim()
  }
  return value
}

async function pair(): Promise<void> {
  const value = sanitizeToken(candidate.value)
  if (!value) return
  busy.value = true
  error.value = ''
  try {
    const auth = await getAuthStatus({ timeoutMs: 5000 })
    if (!auth.configured || auth.corrupt) {
      clearAuthToken()
      emit('retry')
      return
    }
    setAuthToken(value)
    await getStatus({ timeoutMs: 5000 })
    emit('ready')
  } catch (err) {
    clearAuthToken()
    error.value = (err as ApiError)?.status === 401 ? t('authBadToken') : detail(err)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="gate">
    <header class="gate__bar">
      <BrandLogo />
      <div class="gate__tools">
        <button type="button" class="tool" @click="cycleTheme">
          <AppIcon name="themeAuto" :size="14" />
          <span>{{ themeLabel }}</span>
        </button>
        <button
          type="button"
          class="tool"
          @click="setLocale(locale === 'zh' ? 'en' : 'zh')"
        >
          <AppIcon name="language" :size="14" />
          <span>{{ t('appSwitchLanguage') }}</span>
        </button>
      </div>
    </header>

    <main class="gate__main">
      <section class="card">
        <p v-if="state === 'checking'" class="card__lead">{{ t('authChecking') }}</p>

        <template v-else-if="state === 'setup'">
          <template v-if="!issued">
            <h1 class="card__title">
              <AppIcon name="alert" :size="18" /> {{ t('authSetupTitle') }}
            </h1>
            <p class="card__lead">{{ t('authSetupBody') }}</p>
            <p class="card__warn">{{ t('authSetupWarn') }}</p>
            <p v-if="error" class="card__error">{{ error }}</p>
            <button type="button" class="btn btn--primary" :disabled="busy" @click="generate">
              <AppIcon name="check" :size="14" />
              {{ busy ? t('authChecking') : t('authGenerate') }}
            </button>
          </template>
          <template v-else>
            <h1 class="card__title">
              <AppIcon name="check" :size="18" /> {{ t('authIssuedTitle') }}
            </h1>
            <p class="card__lead">{{ t('authIssuedBody') }}</p>
            <div class="token">
              <code class="token__value">{{ issued }}</code>
              <button type="button" class="btn" @click="copy">
                <AppIcon name="check" :size="13" />
                {{ copied ? t('authCopied') : t('authCopy') }}
              </button>
            </div>
            <p class="card__meta">{{ t('authShowOnce') }}</p>
            <label class="ack">
              <input v-model="acknowledged" type="checkbox" />
              <span>{{ t('authAckSaved') }}</span>
            </label>
            <button
              type="button"
              class="btn btn--primary"
              :disabled="!acknowledged"
              @click="enter"
            >
              {{ t('authEnter') }}
            </button>
          </template>
        </template>

        <template v-else-if="state === 'pair'">
          <h1 class="card__title">
            <AppIcon name="device" :size="18" /> {{ t('authPairTitle') }}
          </h1>
          <p class="card__lead">{{ t('authPairBody') }}</p>
          <p v-if="error" class="card__error">{{ error }}</p>
          <TextInput
            v-model="candidate"
            :label="t('authTokenLabel')"
            type="password"
            placeholder="hm1_…"
            wide
            @submit="pair"
          />
          <div class="row">
            <button
              type="button"
              class="btn btn--primary"
              :disabled="busy || !sanitizeToken(candidate)"
              @click="pair"
            >
              {{ t('authPairAction') }}
            </button>
            <button type="button" class="btn" :disabled="busy" @click="emit('retry')">
              <AppIcon name="reload" :size="13" /> {{ t('authRetry') }}
            </button>
          </div>
        </template>

        <template v-else>
          <h1 class="card__title">
            <AppIcon name="fail" :size="18" /> {{ t('authCorruptTitle') }}
          </h1>
          <p class="card__lead">{{ t('authCorruptBody') }}</p>
          <button type="button" class="btn" @click="emit('retry')">
            <AppIcon name="reload" :size="13" /> {{ t('authRetry') }}
          </button>
        </template>

        <p class="card__note">{{ t('authGateNote') }}</p>
      </section>
    </main>
  </div>
</template>

<style scoped>
.gate {
  min-height: 100vh;
  display: flex;
  flex-direction: column;
}

.gate__bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-3);
  padding: var(--sp-4);
  max-width: var(--shell-max);
  width: 100%;
  margin: 0 auto;
}

.gate__tools {
  display: flex;
  gap: var(--sp-2);
}

.tool {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border: 1px solid var(--line);
  background: var(--surface);
  color: var(--muted);
  border-radius: var(--r-sm);
  padding: 0.2rem var(--sp-2);
  font-size: var(--fs-xs);
  cursor: pointer;
}

.gate__main {
  flex: 1;
  display: flex;
  align-items: flex-start;
  justify-content: center;
  padding: var(--sp-5) var(--sp-4) var(--sp-7);
}

.card {
  width: min(560px, 100%);
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--r-md);
  padding: var(--sp-5);
}

.card__title {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-lg);
}

.card__lead {
  color: var(--ink);
  font-size: var(--fs-sm);
  line-height: 1.6;
}

.card__warn {
  color: var(--warn);
  font-size: var(--fs-sm);
  line-height: 1.6;
}

.card__error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.card__meta {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.card__note {
  margin-top: var(--sp-2);
  padding-top: var(--sp-3);
  border-top: 1px solid var(--line);
  color: var(--muted);
  font-size: var(--fs-xs);
}

.token {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.token__value {
  flex: 1;
  min-width: 0;
  word-break: break-all;
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  background: var(--surface-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  padding: var(--sp-2);
  user-select: all;
}

.ack {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-sm);
  color: var(--ink);
}

.row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.btn {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border: 1px solid var(--line);
  background: var(--surface);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.3rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
  align-self: flex-start;
}

.btn:hover:not(:disabled) {
  border-color: var(--muted);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.btn--primary {
  border-color: var(--accent);
  background: var(--accent);
  color: var(--on-accent);
}

.btn--primary:hover:not(:disabled) {
  background: var(--accent-hover);
  border-color: var(--accent-hover);
}
</style>
