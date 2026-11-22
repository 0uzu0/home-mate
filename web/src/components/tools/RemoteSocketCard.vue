<script setup lang="ts">
/**
 * #21 Remote / Computer Socket configuration card.
 *
 * Connects an external computer agent or local-network daemon exposing
 * `GET /tools` and `POST /execute`. Token is write-only; probe and discovery
 * hit the real backend endpoints.
 */
import { computed, ref, watch } from 'vue'
import {
  discoverRemoteSocket,
  getRemoteSocket,
  putRemoteSocket,
  testRemoteSocket,
  type RemoteCapabilitySpec,
  type RemoteSocketProbe,
  type RemoteSocketStatus,
} from '../../api'
import { errorDetail } from '../../format'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import ErrorNotice from '../ErrorNotice.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import PasswordField from '../PasswordField.vue'
import TextInput from '../TextInput.vue'
import NumberField from '../NumberField.vue'

const remote = useResource((signal) => getRemoteSocket({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => remote.load(), 15000)

interface Draft {
  enabled: boolean
  endpointUrl: string
  serviceName: string
  timeoutMs: number
  token: string
  clearToken: boolean
  capabilities: RemoteCapabilitySpec[]
  revision: string
}

const draft = ref<Draft | null>(null)
const probing = ref(false)
const probe = ref<RemoteSocketProbe | null>(null)
const probeError = ref('')
const discovering = ref(false)
const discoverMsg = ref('')
const discoverError = ref('')
const { busy, message, tone, run } = useSave()

function fromStatus(status: RemoteSocketStatus): Draft {
  return {
    enabled: status.enabled,
    endpointUrl: status.endpoint_url ?? '',
    serviceName: status.service_name ?? 'computer',
    timeoutMs: status.timeout_ms ?? 10000,
    token: '',
    clearToken: false,
    capabilities: status.capabilities ?? [],
    revision: status.revision,
  }
}

watch(
  () => remote.data.value,
  (status) => {
    if (!status) return
    if (draft.value === null) draft.value = fromStatus(status)
  },
  { immediate: true },
)

const stateLabel = computed(() => {
  switch (remote.data.value?.state) {
    case 'ready':
      return t('remoteSocketStateReady')
    case 'disabled':
      return t('remoteSocketStateDisabled')
    default:
      return t('remoteSocketStateUnconfigured')
  }
})

const stateTone = computed(() => {
  switch (remote.data.value?.state) {
    case 'ready':
      return 'ok' as const
    case 'disabled':
      return 'warn' as const
    default:
      return 'info' as const
  }
})

async function save(): Promise<void> {
  if (!draft.value) return
  const current = draft.value
  await run(async () => {
    const tokenPatch = current.clearToken
      ? { token: '' }
      : current.token.trim()
        ? { token: current.token.trim() }
        : {}
    const status = await putRemoteSocket({
      enabled: current.enabled,
      endpoint_url: current.endpointUrl.trim(),
      service_name: current.serviceName.trim() || 'computer',
      timeout_ms: current.timeoutMs,
      capabilities: current.capabilities,
      ...tokenPatch,
      revision: current.revision || undefined,
    })
    draft.value = fromStatus(status)
    await remote.load()
  })
}

async function runProbe(): Promise<void> {
  probing.value = true
  probe.value = null
  probeError.value = ''
  try {
    probe.value = await testRemoteSocket()
  } catch (err) {
    probeError.value = errorDetail(err)
  } finally {
    probing.value = false
  }
}

async function runDiscover(): Promise<void> {
  discovering.value = true
  discoverMsg.value = ''
  discoverError.value = ''
  try {
    const res = await discoverRemoteSocket()
    if (!res.ok) {
      discoverError.value = t('remoteSocketDiscoverFail', {
        detail: res.error || (res.http_status ? `HTTP ${res.http_status}` : 'failed'),
      })
      return
    }
    const caps = res.capabilities ?? []
    if (draft.value) draft.value.capabilities = caps
    discoverMsg.value = t('remoteSocketDiscoverOk', {
      n: caps.length,
      requests: res.http_requests,
    })
  } catch (err) {
    discoverError.value = t('remoteSocketDiscoverFail', { detail: errorDetail(err) })
  } finally {
    discovering.value = false
  }
}

function reload(): void {
  void remote.load().then(() => {
    if (remote.data.value) draft.value = fromStatus(remote.data.value)
  })
  probe.value = null
  probeError.value = ''
  discoverMsg.value = ''
  discoverError.value = ''
}
</script>

<template>
  <Panel
    icon="terminal"
    apply="next-turn"
    :title="t('remoteSocketTitle')"
    :hint="t('remoteSocketHint')"
    :tone="remote.data.value?.state === 'ready' ? 'default' : 'warn'"
  >
    <template #actions>
      <Pill :tone="stateTone" size="sm">{{ stateLabel }}</Pill>
      <Pill v-if="remote.data.value" size="sm" mono>
        {{ t('remoteSocketCapabilities', { n: remote.data.value.capabilities.length }) }}
      </Pill>
    </template>

    <ErrorNotice v-if="remote.error.value" :error="remote.error.value" compact @retry="remote.load()" />

    <template v-if="draft">
      <Toggle v-model="draft.enabled" :label="t('remoteSocketEnabled')" />

      <div class="rs__grid">
        <TextInput
          v-model="draft.endpointUrl"
          :label="t('remoteSocketEndpoint')"
          :placeholder="t('remoteSocketEndpointPlaceholder')"
          wide
        />
        <TextInput
          v-model="draft.serviceName"
          :label="t('remoteSocketServiceName')"
          :placeholder="t('remoteSocketServiceNamePlaceholder')"
        />
        <NumberField v-model="draft.timeoutMs" :label="t('remoteSocketTimeout')" :min="500" :max="30000" />
        <PasswordField
          v-model="draft.token"
          :label="t('remoteSocketToken')"
          :is-set="(remote.data.value?.token_set ?? false) && !draft.clearToken"
          :hint="t('remoteSocketTokenHint')"
        />
      </div>

      <label v-if="remote.data.value?.token_set" class="rs__check">
        <input v-model="draft.clearToken" type="checkbox" />
        <span>{{ t('remoteSocketClearToken') }}</span>
      </label>

      <div class="rs__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
        </button>
        <button type="button" class="btn" :disabled="probing" @click="runProbe">
          <AppIcon name="probes" :size="13" /> {{ probing ? t('testing') : t('remoteSocketTest') }}
        </button>
        <button type="button" class="btn" :disabled="discovering" @click="runDiscover">
          <AppIcon name="json" :size="13" />
          {{ discovering ? t('remoteSocketDiscovering') : t('remoteSocketDiscover') }}
        </button>
        <button type="button" class="btn" @click="reload">
          <AppIcon name="reload" :size="13" /> {{ t('reload') }}
        </button>
        <span v-if="message" class="rs__msg" :class="`rs__msg--${tone}`">{{ message }}</span>
      </div>

      <div v-if="probe" class="rs__probe">
        <Pill :tone="probe.ok ? 'ok' : 'danger'" size="sm" mono>
          {{
            probe.ok
              ? t('remoteSocketProbeOk', { status: probe.http_status ?? 200, ms: probe.latency_ms ?? 0 })
              : t('remoteSocketProbeFail', { detail: probe.detail || probe.state })
          }}
        </Pill>
      </div>
      <p v-if="probeError" class="rs__msg rs__msg--error">{{ probeError }}</p>
      <p v-if="discoverMsg" class="rs__msg rs__msg--ok">{{ discoverMsg }}</p>
      <p v-if="discoverError" class="rs__msg rs__msg--error">{{ discoverError }}</p>

      <div class="rs__caps">
        <p class="rs__subtitle">{{ t('remoteSocketCapabilities', { n: draft.capabilities.length }) }}</p>
        <ul v-if="draft.capabilities.length" class="rs__cap-list">
          <li v-for="cap in draft.capabilities" :key="cap.name" class="rs__cap">
            <span class="mono rs__cap-name">{{ cap.name }}</span>
            <Pill :tone="cap.side_effect === 'Mutate' ? 'warn' : 'info'" size="sm" mono>
              {{ cap.side_effect === 'Mutate' ? t('openSocketsEffectMutate') : t('openSocketsEffectQuery') }}
            </Pill>
            <span class="rs__cap-desc">{{ cap.description }}</span>
          </li>
        </ul>
        <p v-else class="rs__msg">{{ t('remoteSocketNoCapabilities') }}</p>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.rs__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr));
  gap: var(--sp-3);
}

.rs__check {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-sm);
  color: var(--muted);
  cursor: pointer;
}

.rs__actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: center;
}

.rs__probe {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: center;
}

.rs__caps {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding-top: var(--sp-2);
  border-top: 1px solid var(--line);
}

.rs__subtitle {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.rs__cap-list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  margin: 0;
  padding: 0;
  list-style: none;
}

.rs__cap {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: baseline;
  padding: var(--sp-1) var(--sp-2);
  background: var(--surface-2);
  border-radius: var(--r-sm);
  font-size: var(--fs-sm);
}

.rs__cap-name {
  font-weight: 500;
}

.rs__cap-desc {
  color: var(--muted);
}

.rs__msg {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.rs__msg--ok {
  color: var(--ok);
}

.rs__msg--error {
  color: var(--danger);
}
</style>
