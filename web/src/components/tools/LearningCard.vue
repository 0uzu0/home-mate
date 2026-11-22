<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { getLearningConfig, putLearningConfig, testLearningConfig } from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useResource } from '../../hooks/useResource'
import Panel from '../Panel.vue'
import Toggle from '../Toggle.vue'
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

const resource = useResource((signal) => getLearningConfig({ signal }), {
  timeoutMs: 5000,
  retries: 1,
})
void resource.load()

const enabled = ref(false)
const url = ref('')
const token = ref('')
const tokenSet = ref(false)
const busy = ref(false)
const message = ref('')
const tone = ref<'ok' | 'error'>('ok')

watch(
  () => resource.data.value,
  (res) => {
    if (!res) return
    enabled.value = res.enabled
    url.value = res.url
    tokenSet.value = res.bearer_token_set
  },
  { immediate: true },
)

const phoneLearnHref = computed(() => {
  const base = url.value.trim().replace(/\/+$/, '')
  return /^https?:\/\//i.test(base) ? `${base}/learn` : ''
})

function stateLabel(state: string): string {
  switch (state) {
    case 'ready':
      return t('searchStateReady')
    case 'disabled':
      return t('searchStateDisabled')
    case 'unconfigured':
      return t('searchStateUnconfigured')
    case 'applied':
      return t('saved')
    default:
      return state
  }
}

async function save(): Promise<void> {
  busy.value = true
  message.value = ''
  try {
    const res = await putLearningConfig({
      enabled: enabled.value,
      url: url.value.trim(),
      ...(token.value.trim() ? { bearer_token: token.value.trim() } : {}),
    })
    url.value = res.url
    tokenSet.value = res.bearer_token_set
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
    const res = await testLearningConfig()
    tone.value = res.ok ? 'ok' : 'error'
    message.value = res.ok ? stateLabel('ready') : res.detail || stateLabel(res.state)
  } catch (err) {
    tone.value = 'error'
    message.value = errorDetail(err)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <Panel icon="provider" :title="t('learningTitle')" :hint="t('learningHint')">
    <LoadingBlock v-if="resource.loading.value && !resource.data.value" :label="t('loading')" />
    <ErrorNotice
      v-else-if="resource.error.value && !resource.data.value"
      :error="resource.error.value"
      @retry="resource.load()"
    />
    <template v-else>
      <Toggle v-model="enabled" :label="t('learningEnabled')" :disabled="busy" />
      <TextInput v-model="url" :label="t('learningUrl')" placeholder="http://<host>:18085" wide />
      <PasswordField
        v-model="token"
        :label="t('personalToken')"
        :is-set="tokenSet"
        :placeholder="tokenSet ? '••••••' : ''"
      />
      <div class="lrn__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          {{ t('learningSave') }}
        </button>
        <button type="button" class="btn" :disabled="busy" @click="testConn">
          {{ t('learningTest') }}
        </button>
        <a
          v-if="phoneLearnHref"
          class="btn"
          :href="phoneLearnHref"
          target="_blank"
          rel="noopener noreferrer"
          referrerpolicy="no-referrer"
        >
          {{ t('learningOpenPhone') }}
        </a>
        <span v-if="message" :class="`lrn__${tone}`" role="status">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.lrn__actions { display: flex; align-items: center; gap: var(--sp-2); flex-wrap: wrap; }
.lrn__ok { color: var(--ok); }
.lrn__error { color: var(--danger); }
</style>
