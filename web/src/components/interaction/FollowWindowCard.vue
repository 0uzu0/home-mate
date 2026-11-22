<script setup lang="ts">
import { ref, watch } from 'vue'
import { getMode, putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Toggle from '../Toggle.vue'
import NumberField from '../NumberField.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()
const enabled = ref(true)
const seconds = ref(20)
const credits = ref(2)
const closeTone = ref(false)
const touched = ref(false)
const { busy, message, tone, run } = useSave()

watch(() => props.mode, mode => {
  if (!mode || touched.value) return
  enabled.value = mode.follow_window_enabled ?? true
  seconds.value = (mode.follow_window_ms ?? 20000) / 1000
  credits.value = mode.followup_credits ?? 2
  closeTone.value = mode.follow_window_tone ?? false
}, { immediate: true })

async function save(): Promise<void> {
  if (!props.device) return
  const patch = {
    follow_window_enabled: enabled.value,
    follow_window_ms: seconds.value * 1000,
    followup_credits: credits.value,
    follow_window_tone: closeTone.value,
  }
  await run(async () => {
    await putMode(props.device!.device, patch)
    const readback = await getMode()
    if (Object.entries(patch).some(([key, value]) => readback[key as keyof ModeSnapshot] !== value)) {
      throw new Error(t('followWindowReadbackError'))
    }
    touched.value = false
    await props.refresh()
  })
}
</script>

<template>
  <Panel icon="wake" apply="immediate" :title="t('followWindowTitle')" :hint="t('followWindowHint')">
    <Toggle v-model="enabled" :label="t('followWindowEnabled')" :disabled="busy" @update:model-value="touched = true" />
    <NumberField v-model="seconds" :label="t('followWindowSeconds')" :min="5" :max="60" :disabled="busy" @update:model-value="touched = true" />
    <NumberField v-model="credits" :label="t('followWindowCredits')" :min="0" :max="5" :disabled="busy" @update:model-value="touched = true" />
    <p class="window__note">{{ t('followWindowBudgetHint') }}</p>
    <Toggle v-model="closeTone" :label="t('followTone')" :hint="t('followToneHint')" :disabled="busy" @update:model-value="touched = true" />
    <div class="window__actions">
      <button type="button" class="window__save" :disabled="busy || !device || !mode" @click="save">
        {{ busy ? t('saving') : t('followWindowSave') }}
      </button>
      <span v-if="message" :class="`window__${tone}`" role="status">{{ message }}</span>
    </div>
  </Panel>
</template>

<style scoped>
.window__note { color: var(--muted); font-size: var(--fs-sm); }
.window__actions { display: flex; align-items: center; gap: var(--sp-2); flex-wrap: wrap; }
.window__save { border: 1px solid var(--accent); border-radius: var(--r-sm); background: var(--accent); color: var(--on-accent); padding: 0.24rem var(--sp-3); cursor: pointer; }
.window__save:disabled { opacity: 0.5; cursor: not-allowed; }
.window__ok { color: var(--ok); }
.window__error { color: var(--danger); }
</style>
