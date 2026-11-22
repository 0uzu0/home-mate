<script setup lang="ts">
/**
 * One provider service (LLM, fallback, ASR or TTS). They share one object
 * shape and one API, so they share one card component; a new provider is a
 * data change, not a new control.
 */
import { computed, ref, watch } from 'vue'
import {
  putService,
  testService,
  type ServiceName,
  type ServiceProbe,
  type ServiceStatus,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'
import Select from '../Select.vue'

const props = defineProps<{
  service: ServiceName
  title: string
  status: ServiceStatus | null
  refresh: () => void | Promise<void>
}>()

interface Draft {
  provider: string
  base_url: string
  model: string
  api_key_env: string
  voice: string
  thinking: string
  api_key: string
}

const draft = ref<Draft | null>(null)
const probe = ref<ServiceProbe | null>(null)
const probing = ref(false)
const probeError = ref('')
const { busy, message, tone, run } = useSave()

function fromStatus(status: ServiceStatus): Draft {
  return {
    provider: status.provider === 'unconfigured' ? '' : status.provider,
    base_url: status.base_url ?? '',
    model: status.model ?? '',
    api_key_env: status.api_key_env ?? '',
    voice: status.voice ?? '',
    thinking: status.thinking ?? '',
    api_key: '',
  }
}

watch(
  () => props.status,
  (status) => {
    if (status && draft.value === null) draft.value = fromStatus(status)
  },
  { immediate: true },
)

async function reload(): Promise<void> {
  if (props.status) {
    draft.value = fromStatus(props.status)
  } else {
    // Nothing loaded yet: make the button fetch instead of doing nothing.
    await props.refresh()
  }
  probe.value = null
  probeError.value = ''
}

async function save(): Promise<void> {
  if (!draft.value) return
  const current = draft.value
  await run(async () => {
    await putService(
      props.service,
      {
        provider: current.provider.trim(),
        base_url: current.base_url.trim(),
        model: current.model.trim(),
        api_key_env: current.api_key_env.trim(),
        voice: props.service === 'tts' ? current.voice.trim() : undefined,
        thinking:
          props.service === 'llm' || props.service === 'llm_fallback' ? current.thinking : undefined,
        api_key: current.api_key ? current.api_key : undefined,
      },
      { timeoutMs: 10000 },
    )
    draft.value = { ...current, api_key: '' }
    await props.refresh()
  })
}

async function runProbe(): Promise<void> {
  probing.value = true
  probe.value = null
  probeError.value = ''
  try {
    probe.value = await testService(props.service)
  } catch (err) {
    probeError.value = errorDetail(err)
  } finally {
    probing.value = false
  }
}

/** Computed so a language switch re-labels the options (audit 2026-11-14). */
const thinkingOptions = computed(() => [
  { value: '', label: t('thinkingDefault') },
  { value: 'disabled', label: t('thinkingDisabled') },
])
</script>

<template>
  <Panel
    icon="provider" apply="next-turn" :title="title" :tone="status?.configured ? 'default' : 'warn'">
    <template #actions>
      <Pill :tone="status?.configured ? 'ok' : 'warn'" size="sm">
        {{ status?.configured ? t('configured') : t('unconfigured') }}
      </Pill>
      <Pill :tone="status?.api_key_set ? 'ok' : 'neutral'" size="sm" mono>
        key: {{ status?.api_key_set ? t('apiKeySet') : t('apiKeyUnset') }}
      </Pill>
    </template>

    <p v-if="status" class="provider__current mono">
      {{ t('current') }}: {{ status.provider || '—' }} · {{ status.model || '—' }}
      <template v-if="status.voice"> · {{ status.voice }}</template>
    </p>

    <div v-if="draft" class="provider__grid">
      <TextInput v-model="draft.provider" :label="t('provider')" placeholder="deepseek / volc / openai-asr" />
      <TextInput v-model="draft.base_url" :label="t('baseUrl')" wide placeholder="https://api.deepseek.com" />
      <TextInput v-model="draft.model" :label="t('model')" placeholder="deepseek-flash" />
      <TextInput v-model="draft.api_key_env" :label="t('apiKeyEnv')" placeholder="DEEPSEEK_API_KEY" />
      <TextInput
        v-if="service === 'tts'"
        v-model="draft.voice"
        :label="t('voice')"
        wide
        :placeholder="t('voicePlaceholder')"
      />
      <Select
        v-if="service === 'llm' || service === 'llm_fallback'"
        v-model="draft.thinking"
        :label="t('thinkingMode')"
        :options="thinkingOptions"
      />
      <PasswordField
        v-model="draft.api_key"
        :label="t('apiKey')"
        :is-set="status?.api_key_set ?? false"
        :hint="t('apiKeyKeep')"
      />
    </div>

    <div class="provider__actions">
      <button type="button" class="btn btn--primary" :disabled="busy || !draft" @click="save">
        <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
      </button>
      <button type="button" class="btn" :disabled="probing" @click="runProbe">
        <AppIcon name="probes" :size="13" /> {{ probing ? t('testing') : t('probe') }}
      </button>
      <button type="button" class="btn" @click="reload"><AppIcon name="reload" :size="13" /> {{ t('reload') }}</button>
      <span v-if="message" class="provider__msg" :class="`provider__msg--${tone}`">{{ message }}</span>
    </div>

    <div v-if="probe" class="provider__probe">
      <Pill :tone="probe.ok ? 'ok' : 'danger'" size="sm" mono>
        {{
          probe.ok
            ? t('probeOk', { reply: probe.reply || probe.text || probe.provider || '', ms: probe.latency_ms ?? '—' })
            : t('probeFail', { detail: probe.error || 'failed' })
        }}
      </Pill>
      <span v-if="probe.ok && probe.audio_ms !== undefined" class="provider__audio mono">
        {{ t('probeAudio', { ms: probe.audio_ms, bytes: probe.bytes ?? 0 }) }}
      </span>
    </div>
    <p v-if="probeError" class="provider__msg provider__msg--error">{{ probeError }}</p>
  </Panel>
</template>

<style scoped>
.provider__current {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.provider__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.provider__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
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

.provider__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.provider__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.provider__probe {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.provider__audio {
  font-size: var(--fs-xs);
  color: var(--muted);
}
</style>
