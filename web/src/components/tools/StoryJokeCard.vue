<script setup lang="ts">
import { ref, onMounted } from 'vue'
import {
  getJokeConfig,
  getRadioConfig,
  getStoryConfig,
  putJokeConfig,
  putRadioConfig,
  putStoryConfig,
  testJokeConfig,
  testRadioConfig,
  testStoryConfig,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import Panel from '../Panel.vue'
import Toggle from '../Toggle.vue'
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'

const storyEnabled = ref(false)
const storyUrl = ref('')
const storyToken = ref('')
const storyTokenSet = ref(false)

const jokeEnabled = ref(false)
const jokeUrl = ref('')
const jokeToken = ref('')
const jokeTokenSet = ref(false)

const radioEnabled = ref(false)
const radioUrl = ref('')
const radioToken = ref('')
const radioTokenSet = ref(false)

const busy = ref(false)
const message = ref('')
const tone = ref<'ok' | 'error'>('ok')

async function load(): Promise<void> {
  try {
    const [s, j, r] = await Promise.all([getStoryConfig(), getJokeConfig(), getRadioConfig()])
    storyEnabled.value = s.enabled
    storyUrl.value = s.url
    storyTokenSet.value = s.bearer_token_set
    jokeEnabled.value = j.enabled
    jokeUrl.value = j.url
    jokeTokenSet.value = j.bearer_token_set
    radioEnabled.value = r.enabled
    radioUrl.value = r.url
    radioTokenSet.value = r.bearer_token_set
  } catch (err) {
    tone.value = 'error'
    message.value = errorDetail(err)
  }
}

async function save(): Promise<void> {
  busy.value = true
  message.value = ''
  try {
    const [s, j, r] = await Promise.all([
      putStoryConfig({
        enabled: storyEnabled.value,
        url: storyUrl.value.trim(),
        ...(storyToken.value.trim() ? { bearer_token: storyToken.value.trim() } : {}),
      }),
      putJokeConfig({
        enabled: jokeEnabled.value,
        url: jokeUrl.value.trim(),
        ...(jokeToken.value.trim() ? { bearer_token: jokeToken.value.trim() } : {}),
      }),
      putRadioConfig({
        enabled: radioEnabled.value,
        url: radioUrl.value.trim(),
        ...(radioToken.value.trim() ? { bearer_token: radioToken.value.trim() } : {}),
      }),
    ])
    storyUrl.value = s.url
    jokeUrl.value = j.url
    radioUrl.value = r.url
    storyTokenSet.value = s.bearer_token_set
    jokeTokenSet.value = j.bearer_token_set
    radioTokenSet.value = r.bearer_token_set
    storyToken.value = ''
    jokeToken.value = ''
    radioToken.value = ''
    tone.value = 'ok'
    message.value = `${s.state} / ${j.state} / ${r.state}`
  } catch (err) {
    tone.value = 'error'
    message.value = errorDetail(err)
  } finally {
    busy.value = false
  }
}

async function testAll(): Promise<void> {
  busy.value = true
  message.value = ''
  try {
    const [s, j, r] = await Promise.all([testStoryConfig(), testJokeConfig(), testRadioConfig()])
    tone.value = s.ok || j.ok || r.ok ? 'ok' : 'error'
    message.value = `story: ${s.ok ? 'ok' : s.detail || s.state} · joke: ${j.ok ? 'ok' : j.detail || j.state} · radio: ${r.ok ? 'ok' : r.detail || r.state}`
  } catch (err) {
    tone.value = 'error'
    message.value = errorDetail(err)
  } finally {
    busy.value = false
  }
}

onMounted(() => {
  void load()
})
</script>

<template>
  <Panel icon="music" :title="t('personalMediaTitle')" :hint="t('personalMediaHint')">
    <div class="pm__group">
      <Toggle v-model="storyEnabled" :label="t('storyEnabled')" :disabled="busy" />
      <TextInput v-model="storyUrl" :label="t('storyUrl')" placeholder="http://192.168.1.10:18083" wide />
      <PasswordField
        v-model="storyToken"
        :label="t('personalToken')"
        :is-set="storyTokenSet"
        :placeholder="storyTokenSet ? '••••••' : ''"
      />
    </div>

    <div class="pm__group">
      <Toggle v-model="jokeEnabled" :label="t('jokeEnabled')" :disabled="busy" />
      <TextInput v-model="jokeUrl" :label="t('jokeUrl')" placeholder="http://192.168.1.10:18081" wide />
      <PasswordField
        v-model="jokeToken"
        :label="t('personalToken')"
        :is-set="jokeTokenSet"
        :placeholder="jokeTokenSet ? '••••••' : ''"
      />
    </div>

    <div class="pm__group">
      <Toggle v-model="radioEnabled" :label="t('radioEnabled')" :disabled="busy" />
      <TextInput v-model="radioUrl" :label="t('radioUrl')" placeholder="http://192.168.1.10:18086" wide />
      <PasswordField
        v-model="radioToken"
        :label="t('personalToken')"
        :is-set="radioTokenSet"
        :placeholder="radioTokenSet ? '••••••' : ''"
      />
    </div>

    <div class="pm__actions">
      <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
        {{ t('personalSave') }}
      </button>
      <button type="button" class="btn" :disabled="busy" @click="testAll">
        {{ t('personalTest') }}
      </button>
      <span v-if="message" :class="`pm__${tone}`" role="status">{{ message }}</span>
    </div>
  </Panel>
</template>

<style scoped>
.pm__group { display: flex; flex-direction: column; gap: var(--sp-2); padding-bottom: var(--sp-2); border-bottom: 1px solid var(--line); }
.pm__actions { display: flex; align-items: center; gap: var(--sp-2); flex-wrap: wrap; }
.pm__ok { color: var(--ok); }
.pm__error { color: var(--danger); }
</style>
