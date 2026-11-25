<script setup lang="ts">
/**
 * Setup wizard (#13): streamlined 3-step flow:
 * (1) Pairing / Auth, (2) Network (Wi-Fi), (3) One working LLM model,
 * plus a built-in one-sentence verification action and Wi-Fi provisioning banner.
 * ASR, TTS, Tools, and Home Assistant stay in ServicePage / ToolPage.
 */
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { RouterLink } from 'vue-router'
import { getAuthStatus, getStatus, isAbort, postChat, provisionAuth, resolveWifiProvisioningForDevice } from '../api'
import { authToken, setAuthToken } from '../auth'
import { t } from '../i18n'
import { useResource } from '../hooks/useResource'
import {
  devices as modeDevices,
  gatewayDevices,
  loading as storeLoading,
  refresh as refreshDevices,
  selected,
  wifiProvisioningByDevice,
} from '../stores/devices'
import AppIcon from '../components/AppIcon.vue'
import Panel from '../components/Panel.vue'
import WizardStepCard from '../components/wizard/WizardStepCard.vue'
import WizardWifiCard from '../components/wizard/WizardWifiCard.vue'
import WizardServiceCard from '../components/wizard/WizardServiceCard.vue'
import LoadingBlock from '../components/LoadingBlock.vue'
import ErrorNotice from '../components/ErrorNotice.vue'

const VERIFIED_STORAGE_KEY = 'hm.setup.llm_verified'

function readPersistedVerified(): boolean | undefined {
  try {
    const raw = localStorage.getItem(VERIFIED_STORAGE_KEY)
    if (raw === 'true') return true
    if (raw === 'false') return false
    return undefined
  } catch {
    return undefined
  }
}

const auth = useResource((signal) => getAuthStatus({ signal }), { timeoutMs: 5000, retries: 1 })
const wifiId = ref<string | null>(null)
const status = useResource(
  (signal) => getStatus({ signal, device: wifiId.value || selected.value?.device || undefined }),
  { timeoutMs: 6000, retries: 1 },
)
const provisioning = ref(false)
const authError = ref('')

async function reload(): Promise<void> {
  await Promise.all([auth.load(), status.load(), refreshDevices()])
}
void reload()

const authDone = computed(
  () => auth.data.value?.configured === true && !auth.data.value?.corrupt && !!authToken.value,
)
const authSummary = computed(() => {
  if (auth.data.value?.corrupt) return t('authCorruptTitle')
  return authDone.value ? `${t('setupAuthDone')} (hm1_••••)` : t('setupAuthPending')
})

async function provision(): Promise<void> {
  provisioning.value = true
  authError.value = ''
  try {
    const resp = await provisionAuth({ timeoutMs: 8000 })
    setAuthToken(resp.token)
    await reload()
  } catch (err) {
    authError.value = err instanceof Error ? err.message : String(err)
  } finally {
    provisioning.value = false
  }
}

const wifiDeviceIds = computed<string[]>(() => {
  const seen = new Set<string>()
  const out: string[] = []
  for (const d of modeDevices.value) {
    if (d.device && !seen.has(d.device)) {
      seen.add(d.device)
      out.push(d.device)
    }
  }
  for (const d of gatewayDevices.value) {
    if (d.stable_id && !seen.has(d.stable_id)) {
      seen.add(d.stable_id)
      out.push(d.stable_id)
    }
  }
  return out
})

watch(
  [wifiDeviceIds, selected],
  ([ids, sel], [, prevSel]) => {
    const selectedChanged = prevSel !== undefined && sel?.device !== prevSel?.device
    if (selectedChanged && sel?.device) {
      wifiId.value = sel.device
      return
    }
    if (!wifiId.value || (ids.length > 0 && !ids.includes(wifiId.value))) {
      wifiId.value = sel?.device ?? (ids.length === 1 ? ids[0] : null)
    }
  },
  { immediate: true },
)

watch(wifiId, (id, prev) => {
  if (prev !== undefined && id !== prev) {
    void status.load()
  }
})

const wifiGateway = computed(() => {
  if (wifiId.value) {
    return gatewayDevices.value.find((device) => device.stable_id === wifiId.value) ?? null
  }
  return gatewayDevices.value.length === 1 ? gatewayDevices.value[0] : null
})
const online = computed(() => wifiGateway.value?.online ?? false)
const ssid = computed(() => wifiGateway.value?.wifi_ssid ?? null)

const llm = computed(() => status.data.value?.services.llm ?? null)
const asr = computed(() => status.data.value?.services.asr ?? null)
const tts = computed(() => status.data.value?.services.tts ?? null)
const voiceReady = computed(() => !!asr.value?.configured && !!tts.value?.configured)

const wifiNotice = computed(() => {
  const resolved = resolveWifiProvisioningForDevice(
    wifiId.value ?? selected.value?.device,
    status.data.value,
    wifiProvisioningByDevice.value,
    wifiDeviceIds.value.length,
  )
  return resolved.requested ? resolved : null
})

const wifiSummary = computed(() => {
  if (!wifiGateway.value) return t('setupDeviceMissing')
  if (!online.value) return t('setupOffline')
  return `${wifiGateway.value.stable_id} · ${t('setupOnline')} · ${
    ssid.value ? `SSID: ${ssid.value}` : t('setupSsidUnknown')
  }`
})

const verified = ref<Record<string, boolean | undefined>>({
  llm: readPersistedVerified(),
})

function markVerified(service: string, ok: boolean): void {
  verified.value = { ...verified.value, [service]: ok }
  if (service === 'llm') {
    try {
      localStorage.setItem(VERIFIED_STORAGE_KEY, String(ok))
    } catch {
      /* private mode */
    }
  }
  void reload()
}

function serviceSummary(key: 'llm', service: typeof llm.value): string {
  if (!service?.configured) return t('setupUnconfigured')
  const probe = verified.value[key]
  if (probe === true) return `${service.provider} · ${service.model} · ${t('setupVerified')}`
  if (probe === false) return `${service.provider} · ${t('setupProbeFailed')}`
  return `${service.provider} · ${service.model} · ${t('setupDone')}`
}

function serviceDone(key: 'llm', service: typeof llm.value): boolean {
  return !!service?.configured && verified.value[key] !== false
}

const pending = computed(() => {
  let count = 0
  if (!authDone.value) count += 1
  if (!online.value) count += 1
  if (!serviceDone('llm', llm.value)) count += 1
  return count
})

// Built-in 1-sentence verification (#13)
const verifyPrompt = ref('')
const verifyBusy = ref(false)
const verifyReply = ref('')
const verifyError = ref('')
let verifyController: AbortController | null = null

async function runQuickVerify(): Promise<void> {
  const prompt = (verifyPrompt.value.trim() || t('setupVerifyDefaultPrompt')).trim()
  verifyController?.abort()
  const controller = new AbortController()
  verifyController = controller
  verifyBusy.value = true
  verifyReply.value = ''
  verifyError.value = ''
  let sawError = false
  try {
    await postChat(
      'setup-verify',
      prompt,
      (ev) => {
        if ((ev.type === 'token' || ev.type === 'fact') && typeof ev.text === 'string') {
          verifyReply.value += ev.text
        } else if (ev.type === 'answer' && typeof ev.text === 'string' && !verifyReply.value) {
          verifyReply.value = ev.text
        } else if (ev.type === 'error' && typeof ev.message === 'string') {
          sawError = true
          verifyError.value = ev.message
        } else if (ev.type === 'turn_end' && ev.status === 'failed' && !verifyError.value) {
          sawError = true
          verifyError.value = t('setupVerifyFailed')
        }
      },
      { timeoutMs: 45000, signal: controller.signal },
    )
    if (verifyReply.value.trim() && !sawError && !verifyError.value) {
      markVerified('llm', true)
    }
  } catch (err) {
    if (!controller.signal.aborted && !isAbort(err)) {
      verifyError.value = err instanceof Error ? err.message : String(err)
    }
  } finally {
    if (verifyController === controller) {
      verifyController = null
      verifyBusy.value = false
    }
  }
}

onBeforeUnmount(() => {
  verifyController?.abort()
  verifyController = null
})

const loading = computed(
  () => !status.data.value && (status.loading.value || storeLoading.value),
)
const firstError = computed(() => status.error.value)
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('setupTitle') }}</h1>
        <p class="page__sub">{{ t('setupSub') }}</p>
        <p class="page__pending" :class="{ 'page__pending--ok': pending === 0 && voiceReady }">
          {{
            pending === 0 && voiceReady
              ? t('setupAllDone')
              : pending === 0 && !voiceReady
                ? t('setupVoicePending')
                : t('setupPending', { n: pending })
          }}
        </p>
      </div>
      <RouterLink class="page__back" to="/settings">
        <AppIcon name="back" :size="14" /> {{ t('backToSettings') }}
      </RouterLink>
    </header>

    <div v-if="wifiNotice" class="wifi-banner" role="status">
      <div class="wifi-banner__body">
        <strong>{{ t('wifiProvisioningBannerTitle') }}</strong>
        <p>{{ t('wifiProvisioningBannerHint', { device: wifiNotice.device || 'dot2' }) }}</p>
      </div>
    </div>

    <ErrorNotice v-if="firstError && !status.data.value" :error="firstError" @retry="reload" />
    <LoadingBlock v-else-if="loading" :label="t('loading')" />

    <div v-else class="page__steps">
      <WizardStepCard :index="1" :title="t('authStepTitle')" :done="authDone" :summary="authSummary">
        <div class="wiz__auth">
          <button
            v-if="auth.data.value && !auth.data.value.configured"
            type="button"
            class="btn"
            :disabled="provisioning"
            @click="provision"
          >
            {{ provisioning ? t('setupWorking') : t('authGenerate') }}
          </button>
          <RouterLink v-else to="/settings" class="wiz__link">{{ t('setupAuthManage') }}</RouterLink>
          <p v-if="authError" class="wiz__err">{{ authError }}</p>
        </div>
      </WizardStepCard>

      <WizardStepCard :index="2" :title="t('wifiTitle')" :done="online" :summary="wifiSummary">
        <label v-if="wifiDeviceIds.length > 1" class="page__select">
          <span>{{ t('statusSelectedDevice') }}</span>
          <select v-model="wifiId">
            <option v-for="id in wifiDeviceIds" :key="id" :value="id">
              {{ id }}
            </option>
          </select>
        </label>
        <WizardWifiCard :key="wifiId ?? 'none'" :device-id="wifiGateway?.stable_id ?? ''" :online="online" @done="reload" />
        <p v-if="!online" class="wiz__offline-hint">{{ t('setupWifiOfflineGuide') }}</p>
      </WizardStepCard>

      <WizardStepCard
        :index="3"
        :title="t('llmTitle')"
        :done="serviceDone('llm', llm)"
        :saved="!!llm?.configured"
        :summary="serviceSummary('llm', llm)"
      >
        <WizardServiceCard service="llm" :status="llm" @done="(ok) => markVerified('llm', ok)" />
      </WizardStepCard>

      <Panel icon="chat" :title="t('setupVerifyTitle')" :hint="t('setupVerifyHint')">
        <form class="verify__row" @submit.prevent="runQuickVerify">
          <input
            v-model="verifyPrompt"
            type="text"
            class="verify__input"
            :placeholder="t('setupVerifyDefaultPrompt')"
            :disabled="verifyBusy"
          />
          <button type="submit" class="btn btn--primary" :disabled="verifyBusy || !llm?.configured">
            {{ verifyBusy ? t('setupVerifySending') : t('setupVerifySend') }}
          </button>
        </form>
        <p v-if="verifyReply" class="verify__reply">
          <strong>{{ t('setupVerifyReplyLabel') }}</strong> {{ verifyReply }}
        </p>
        <p v-if="verifyError" class="wiz__err">{{ verifyError }}</p>
      </Panel>

      <div v-if="!voiceReady" class="voice-check" role="status">
        <strong>{{ t('setupVoiceCheckTitle') }}</strong>
        <p>{{ t('setupVoiceCheckBody') }}</p>
        <ul class="voice-check__list">
          <li>
            <span>{{ t('setupVoiceAsr') }}:</span>
            <strong>{{ asr?.configured ? t('setupDone') : t('setupUnconfigured') }}</strong>
          </li>
          <li>
            <span>{{ t('setupVoiceTts') }}:</span>
            <strong>{{ tts?.configured ? t('setupDone') : t('setupUnconfigured') }}</strong>
          </li>
        </ul>
        <RouterLink to="/services" class="wiz__link">{{ t('setupOpenServices') }}</RouterLink>
      </div>

      <p class="page__optional">
        {{ t('setupOptionalHint') }}
        <RouterLink to="/services">{{ t('setupOpenServices') }}</RouterLink>
        /
        <RouterLink to="/tools">{{ t('setupOpenTools') }}</RouterLink>.
      </p>
    </div>
  </section>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.page__head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.page__title {
  font-size: var(--fs-xl);
}

.page__sub {
  margin-top: var(--sp-1);
  color: var(--muted);
  font-size: var(--fs-sm);
  max-width: 75ch;
}

.page__pending {
  margin-top: var(--sp-2);
  color: var(--warn);
  font-size: var(--fs-sm);
}

.page__pending--ok {
  color: var(--ok);
}

.page__back {
  font-size: var(--fs-sm);
}

.wifi-banner {
  border: 1px solid var(--warn);
  background: var(--surface-2);
  border-radius: var(--r-md);
  padding: var(--sp-3) var(--sp-4);
  font-size: var(--fs-sm);
}

.wifi-banner__body p {
  margin-top: var(--sp-1);
  color: var(--muted);
}

.page__select {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-sm);
  color: var(--muted);
}

.page__select select {
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.2rem var(--sp-2);
}

.page__steps {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.wiz__auth {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.wiz__link,
.page__optional a {
  color: var(--accent);
  font-size: var(--fs-sm);
}

.wiz__err {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.wiz__offline-hint {
  margin-top: var(--sp-2);
  padding: var(--sp-2) var(--sp-3);
  border-left: 3px solid var(--warn);
  background: var(--surface-2);
  color: var(--ink-2);
  font-size: var(--fs-sm);
  border-radius: var(--r-xs);
}

.voice-check {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3) var(--sp-4);
  border: 1px solid var(--warn-line);
  background: var(--warn-soft);
  border-radius: var(--r-md);
  font-size: var(--fs-sm);
}

.voice-check__list {
  display: flex;
  gap: var(--sp-4);
  flex-wrap: wrap;
}

.verify__row {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.verify__input {
  flex: 1 1 18rem;
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.28rem var(--sp-3);
  font-size: var(--fs-sm);
}

.verify__reply {
  margin-top: var(--sp-2);
  font-size: var(--fs-sm);
  color: var(--ink-2);
}

.page__optional {
  color: var(--muted);
  font-size: var(--fs-sm);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.24rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn--primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
