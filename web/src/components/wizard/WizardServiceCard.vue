<script setup lang="ts">
/**
 * One provider step of the wizard. It does exactly what the Services page
 * does — PUT then probe — because a wizard that configures things differently
 * is a second, unverified configuration path.
 */
import { ref } from 'vue'
import { putService, testService, type ServiceName, type ServiceStatus } from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import TextInput from '../TextInput.vue'

const props = defineProps<{
  service: Extract<ServiceName, 'llm' | 'asr' | 'tts'>
  status: ServiceStatus | null
}>()
const emit = defineEmits<{ done: [verified: boolean] }>()

const defaults = {
  llm: { provider: 'deepseek', base_url: 'https://api.deepseek.com', model: 'deepseek-flash', voice: '', extra: 'seed-tts-2.0' },
  asr: {
    provider: 'openai-asr',
    base_url: 'https://api.siliconflow.cn/v1',
    model: 'FunAudioLLM/SenseVoiceSmall',
    voice: '',
    extra: '',
  },
  tts: { provider: 'volc', base_url: '', model: 'seed-tts-2.0', voice: 'zh_female_xiaohe_uranus_bigtts', extra: '' },
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
    extra: d.extra,
  }
}

const form = ref({ ...fromStatus() })
const apiKey = ref('')
/** Carried through untouched so a key referenced by environment variable
 * survives a wizard save. */
const apiKeyEnv = ref(props.status?.api_key_env || '')
const busy = ref(false)
const result = ref('')
const failed = ref(false)

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
      <TextInput v-model="form.base_url" :label="t('baseUrl')" wide :placeholder="defaults[service].base_url" />
      <TextInput v-model="form.model" :label="t('model')" :placeholder="defaults[service].model" />
      <TextInput
        v-if="service === 'tts'"
        v-model="form.voice"
        :label="t('ttsVoice')"
        wide
        :placeholder="defaults.tts.voice"
      />
      <TextInput v-model="apiKey" :label="t('apiKey')" type="password" :placeholder="status?.api_key_set ? t('apiKeySet') : t('apiKeyUnset')" />
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
