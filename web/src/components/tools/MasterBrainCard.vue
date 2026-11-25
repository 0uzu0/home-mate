<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import {
  getMasterBrainConfig,
  putMasterBrainConfig,
  testMasterBrainConfig,
  type MasterBrainProtocol,
  type MasterBrainTaskRecord,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useResource } from '../../hooks/useResource'
import Panel from '../Panel.vue'
import Toggle from '../Toggle.vue'
import Select from '../Select.vue'
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

const resource = useResource((signal) => getMasterBrainConfig({ signal }), {
  timeoutMs: 5000,
  retries: 1,
})
void resource.load()

const enabled = ref(false)
const protocol = ref<MasterBrainProtocol>('hermes')
const url = ref('')
const callbackUrl = ref('')
const token = ref('')
const tokenSet = ref(false)
const stateWord = ref('unconfigured')
const recentTasks = ref<MasterBrainTaskRecord[]>([])
const busy = ref(false)
const message = ref('')
const tone = ref<'ok' | 'error'>('ok')

const protocolOptions = computed(() => [
  { value: 'hermes', label: t('masterBrainProtocolHermes') },
  { value: 'generic', label: t('masterBrainProtocolGeneric') },
])

const urlPlaceholder = computed(() =>
  protocol.value === 'hermes' ? 'http://<hermes-host>:8642' : 'http://<bridge-host>:18940',
)

watch(
  () => resource.data.value,
  (res) => {
    if (!res) return
    enabled.value = res.enabled
    protocol.value = res.protocol === 'generic' ? 'generic' : 'hermes'
    url.value = res.url
    callbackUrl.value = res.callback_url
    tokenSet.value = res.bearer_token_set
    stateWord.value = res.state
    recentTasks.value = res.recent_tasks ?? []
  },
  { immediate: true },
)

function stateLabel(state: string): string {
  switch (state) {
    case 'ready':
      return t('searchStateReady')
    case 'disabled':
      return t('searchStateDisabled')
    case 'unconfigured':
      return t('searchStateUnconfigured')
    default:
      return state
  }
}

async function save(): Promise<void> {
  busy.value = true
  message.value = ''
  try {
    const res = await putMasterBrainConfig({
      enabled: enabled.value,
      protocol: protocol.value,
      url: url.value.trim(),
      callback_url: protocol.value === 'generic' ? callbackUrl.value.trim() : '',
      ...(token.value.trim() ? { bearer_token: token.value.trim() } : {}),
    })
    protocol.value = res.protocol === 'generic' ? 'generic' : 'hermes'
    url.value = res.url
    callbackUrl.value = res.callback_url
    tokenSet.value = res.bearer_token_set
    stateWord.value = res.state
    recentTasks.value = res.recent_tasks ?? []
    token.value = ''
    tone.value = 'ok'
    message.value = stateLabel(res.state)
  } catch (err) {
    tone.value = 'error'
    message.value = errorDetail(err)
  } finally {
    busy.value = false
  }
}

async function testConn(): Promise<void> {
  busy.value = true
  message.value = ''
  try {
    const res = await testMasterBrainConfig()
    tone.value = res.ok ? 'ok' : 'error'
    if (res.ok) {
      message.value = t('masterBrainTestOk', { ms: res.latency_ms ?? 0 })
    } else {
      message.value = res.detail || stateLabel(res.state)
    }
  } catch (err) {
    tone.value = 'error'
    message.value = errorDetail(err)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <Panel icon="provider" :title="t('masterBrainTitle')" :hint="t('masterBrainHint')">
    <LoadingBlock v-if="resource.loading.value && !resource.data.value" :label="t('loading')" />
    <ErrorNotice
      v-else-if="resource.error.value && !resource.data.value"
      :error="resource.error.value"
      @retry="resource.load()"
    />
    <template v-else>
      <div class="mb__meta">
        <span class="mb__pill" :class="`mb__pill--${stateWord}`">{{ stateLabel(stateWord) }}</span>
      </div>
      <Toggle v-model="enabled" :label="t('masterBrainEnabled')" :disabled="busy" />
      <Select
        v-model="protocol"
        :label="t('masterBrainProtocol')"
        :options="protocolOptions"
        :disabled="busy"
      />
      <TextInput
        v-model="url"
        :label="t('masterBrainUrl')"
        :placeholder="urlPlaceholder"
        wide
      />
      <TextInput
        v-if="protocol === 'generic'"
        v-model="callbackUrl"
        :label="t('masterBrainCallbackUrl')"
        placeholder="http://<speaker-ip>:8930"
        wide
      />
      <PasswordField
        v-model="token"
        :label="t('masterBrainToken')"
        :is-set="tokenSet"
        :placeholder="tokenSet ? '••••••' : ''"
      />
      <div class="mb__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          {{ t('masterBrainSave') }}
        </button>
        <button type="button" class="btn" :disabled="busy" @click="testConn">
          {{ t('masterBrainTest') }}
        </button>
        <span v-if="message" :class="`mb__${tone}`" role="status">{{ message }}</span>
      </div>
      <div v-if="recentTasks.length" class="mb__tasks">
        <div class="mb__tasks-title">{{ t('masterBrainRecentTasks', { n: recentTasks.length }) }}</div>
        <ul class="mb__task-list">
          <li v-for="task in recentTasks.slice(0, 5)" :key="task.task_id" class="mb__task">
            <span class="mb__task-status">{{ task.status }}</span>
            <span v-if="task.last_tool" class="mb__task-tool">{{ task.last_tool }}</span>
            <span class="mb__task-summary">{{ task.summary }}</span>
            <code class="mb__task-id">{{ task.task_id }}</code>
          </li>
        </ul>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.mb__meta { display: flex; align-items: center; gap: var(--sp-2); }
.mb__pill {
  display: inline-flex;
  align-items: center;
  padding: 2px 8px;
  border-radius: 999px;
  font-size: var(--fs-xs);
  background: var(--surface-2);
  color: var(--muted);
}
.mb__pill--ready { color: var(--ok); }
.mb__actions { display: flex; align-items: center; gap: var(--sp-2); flex-wrap: wrap; }
.mb__ok { color: var(--ok); }
.mb__error { color: var(--danger); }
.mb__tasks {
  margin-top: var(--sp-2);
  padding-top: var(--sp-2);
  border-top: 1px solid var(--line);
}
.mb__tasks-title { font-size: var(--fs-xs); color: var(--muted); margin-bottom: var(--sp-1); }
.mb__task-list { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 4px; }
.mb__task { display: flex; align-items: center; gap: var(--sp-2); font-size: var(--fs-sm); }
.mb__task-status { font-size: var(--fs-xs); color: var(--muted); min-width: 64px; }
.mb__task-tool {
  font-size: var(--fs-xs);
  padding: 1px 6px;
  border-radius: 4px;
  background: var(--surface-2);
  color: var(--muted);
}
.mb__task-summary { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mb__task-id { font-size: var(--fs-xs); color: var(--muted); }
</style>
