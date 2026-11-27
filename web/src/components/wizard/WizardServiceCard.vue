<script setup lang="ts">
/**
 * One provider step of the wizard. It does exactly what the Services page
 * does — PUT then probe — because a wizard that configures things differently
 * is a second, unverified configuration path.
 */
import { computed, ref, watch } from 'vue'
import { putService, testService, type ServiceName, type ServiceStatus } from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import Select from '../Select.vue'
import TextInput from '../TextInput.vue'

const props = defineProps<{
  service: Extract<ServiceName, 'llm' | 'asr' | 'tts'>
  status: ServiceStatus | null
}>()
const emit = defineEmits<{ done: [verified: boolean] }>()

const defaults = {
  llm: { provider: 'deepseek', base_url: 'https://api.deepseek.com/v1', model: 'deepseek-chat', voice: '' },
  asr: {
    provider: 'openai-asr',
    base_url: 'https://api.siliconflow.cn/v1',
    model: 'FunAudioLLM/SenseVoiceSmall',
    voice: '',
  },
  tts: { provider: 'volc', base_url: '', model: 'seed-tts-2.0', voice: 'zh_female_xiaohe_uranus_bigtts' },
}

/**
 * Start from the SAVED configuration, not from the hardcoded defaults
 * (audit 2026-11-14, P1): revisiting the wizard used to overwrite a working
 * provider/endpoint/model with the defaults, and because the PUT omitted
 * `api_key_env` the route reset it to empty as well.
 */
function fromStatus(): typeof defaults.llm {
  const d = defaults[props.service]
  const st = props.status
  if (!st || !st.configured) return { ...d }
  return {
    provider: st.provider || d.provider,
    base_url: st.base_url || d.base_url,
    model: st.model || d.model,
    voice: st.voice || d.voice,
  }
}

const form = ref({ ...fromStatus() })
const apiKey = ref('')
/** Carried through untouched so a key referenced by environment variable
 * survives a wizard save. */
const apiKeyEnv = ref(props.status?.api_key_env || '')
const dirty = ref(false)
const busy = ref(false)
const result = ref('')
const failed = ref(false)

watch(
  () => props.status,
  (st) => {
    if (!dirty.value && st) {
      form.value = { ...fromStatus() }
      apiKeyEnv.value = st.api_key_env || ''
    }
  },
  { deep: true },
)

const ttsProviderOptions = computed(() => {
  const opts = [
    { value: 'volc', label: t('providerTtsVolc') },
    { value: 'omni', label: t('providerTtsOmni') },
  ]
  const cur = form.value.provider
  if (cur && !opts.some((o) => o.value === cur)) opts.push({ value: cur, label: cur })
  return opts
})

const baseUrlPlaceholder = computed(() => {
  if (props.service === 'tts') {
    return form.value.provider === 'volc' ? t('baseUrlPlaceholderVolc') : 'https://api.siliconflow.cn/v1'
  }
  return defaults[props.service].base_url
})

const baseUrlHint = computed(() => {
  if (props.service === 'asr') return t('baseUrlHintAsr')
  if (props.service === 'tts') {
    return form.value.provider === 'volc' ? t('baseUrlHintTtsVolc') : t('baseUrlHintTtsOmni')
  }
  return t('baseUrlHintLlm')
})

const modelPlaceholder = computed(() => {
  if (props.service === 'tts') {
    return form.value.provider === 'volc' ? 'seed-tts-2.0' : 'FunAudioLLM/CosyVoice2-0.5B / tts-1'
  }
  return defaults[props.service].model
})

const voicePlaceholder = computed(() =>
  form.value.provider === 'volc' ? t('voicePlaceholderVolc') : t('voicePlaceholderOmni'),
)

function onTtsProviderChange(next: string): void {
  form.value.provider = next
  dirty.value = true
  if (next === 'omni' && form.value.model === 'seed-tts-2.0') {
    form.value.model = ''
  }
  if (next === 'omni' && form.value.voice === defaults.tts.voice) {
    form.value.voice = ''
  }
}

async function saveAndTest(): Promise<void> {
  busy.value = true
  result.value = ''
  failed.value = false
  try {
    await putService(props.service, {
      provider: form.value.provider,
      base_url: form.value.base_url || undefined,
      model: form.value.model,
      voice: props.service === 'tts' ? form.value.voice : undefined,
      ...(props.service === 'llm' && props.status?.thinking !== undefined ? { thinking: props.status.thinking } : {}),
      api_key: apiKey.value || undefined,
      ...(apiKeyEnv.value ? { api_key_env: apiKeyEnv.value } : {}),
    })
    const probe = await testService(props.service)
    if (probe.ok) {
      const extra =
        props.service === 'tts'
          ? ` · ${Math.round((probe.audio_ms ?? 0) / 100) / 10}s`
          : props.service === 'asr' && probe.text
            ? ` · ${probe.text}`
            : probe.reply
              ? ` · ${probe.reply}`
              : ''
      result.value = `✓ ${probe.latency_ms ?? '—'} ms${extra}`
    } else {
      result.value = `✗ ${probe.error ?? 'failed'}`
      failed.value = true
    }
    emit('done', probe.ok)
  } catch (err) {
    result.value = `✗ ${errorDetail(err)}`
    failed.value = true
    emit('done', false)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="wizsvc">
    <div class="wizsvc__row">
      <Select
        v-if="service === 'tts'"
        :model-value="form.provider"
        :label="t('provider')"
        :options="ttsProviderOptions"
        @update:model-value="onTtsProviderChange"
      />
      <TextInput
        v-model="form.base_url"
        :label="t('baseUrl')"
        :hint="baseUrlHint"
        wide
        :placeholder="baseUrlPlaceholder"
        @update:model-value="dirty = true"
      />
      <TextInput v-model="form.model" :label="t('model')" :placeholder="modelPlaceholder" @update:model-value="dirty = true" />
      <TextInput
        v-if="service === 'tts'"
        v-model="form.voice"
        :label="t('ttsVoice')"
        wide
        :placeholder="voicePlaceholder"
        @update:model-value="dirty = true"
      />
      <TextInput v-model="apiKey" :label="t('apiKey')" type="password" :placeholder="status?.api_key_set ? t('apiKeySet') : t('apiKeyUnset')" @update:model-value="dirty = true" />
      <button type="button" class="btn" :disabled="busy" @click="saveAndTest">
        {{ busy ? t('setupWorking') : t('setupSaveTest') }}
      </button>
    </div>
    <p v-if="result" class="wizsvc__result" :class="{ 'wizsvc__result--bad': failed }">{{ result }}</p>
  </div>
</template>

<style scoped>
.wizsvc {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.wizsvc__row {
  display: flex;
  align-items: end;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.wizsvc__result {
  color: var(--ok);
  font-size: var(--fs-sm);
  word-break: break-word;
}

.wizsvc__result--bad {
  color: var(--danger);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.28rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
