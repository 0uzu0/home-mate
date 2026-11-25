<script setup lang="ts">
import { ref, watch } from 'vue'
import { getMode, putPrompts, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { advancedMode } from '../../stores/advanced'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
const props = defineProps<{ mode: ModeSnapshot | null; refresh: () => void | Promise<void> }>()
const assistantName = ref('')
const persona = ref('')
const override = ref('')
const touched = ref(false)
const { busy, message, tone, run } = useSave()
watch(() => props.mode, mode => {
  if (!mode || touched.value) return
  assistantName.value = mode.assistant_name ?? ''
  persona.value = mode.persona_prompt ?? ''
  override.value = mode.system_prompt_override ?? ''
}, { immediate: true })
async function save(): Promise<void> {
  const patch = {
    assistant_name: assistantName.value.trim(),
    persona_prompt: persona.value.trim(),
    system_prompt_override: override.value.trim() || null,
  }
  await run(async () => {
    await putPrompts(patch)
    const mode = await getMode()
    if ((mode.assistant_name ?? '') !== patch.assistant_name || mode.persona_prompt !== patch.persona_prompt || (mode.system_prompt_override ?? null) !== patch.system_prompt_override) throw new Error(t('promptReadback'))
    touched.value = false
    await props.refresh()
  })
}
</script>
<template>
  <Panel icon="settings" apply="next-turn" :title="t('promptTitle')" :hint="t('promptHint')">
    <template #actions>
      <Pill size="sm" tone="info" mono>{{ t('scopeNodeGlobal') }}</Pill>
    </template>
    <label class="field">{{ t('promptAssistantName') }}
      <input v-model="assistantName" type="text" maxlength="64" placeholder="Home Mate" :disabled="busy" @input="touched = true" />
      <span>{{ t('promptAssistantNameHint') }}</span>
    </label>
    <label class="field">{{ t('promptPersona') }}
      <textarea v-model="persona" rows="5" :disabled="busy" @input="touched = true" />
      <span>{{ [...persona].length }} / 4000 · {{ t('promptPersonaHint') }}</span>
    </label>
    <p v-if="advancedMode || mode?.system_prompt_override">{{ mode?.system_prompt_override ? t('promptActive') : t('promptDefault') }}</p>
    <details v-if="advancedMode">
      <summary>{{ t('promptAdvanced') }}</summary>
      <p class="warning" role="note">{{ t('promptWarning') }}</p>
      <label class="field">{{ t('promptOverride') }}
        <textarea v-model="override" rows="7" :disabled="busy" @input="touched = true" />
        <span>{{ [...override].length }} / 4000</span>
      </label>
    </details>
    <div class="actions">
      <button type="button" :disabled="busy || !mode || [...assistantName.trim()].length > 64 || [...persona].length > 4000 || [...override].length > 4000" @click="save">{{ busy ? t('saving') : t('promptSave') }}</button>
      <span v-if="message" role="status" :class="tone">{{ message }}</span>
    </div>
  </Panel>
</template>
<style scoped>
.field { display: flex; flex-direction: column; gap: var(--sp-2); }
input, textarea { width: 100%; box-sizing: border-box; border: 1px solid var(--line); border-radius: var(--r-sm); background: var(--bg); color: var(--ink); font: inherit; padding: var(--sp-2); }
textarea { resize: vertical; }
.field span { color: var(--muted); font-size: var(--fs-sm); }
summary { cursor: pointer; } .warning { color: var(--warn); margin: var(--sp-2) 0; }
.actions { display: flex; gap: var(--sp-2); align-items: center; flex-wrap: wrap; }
button { border: 1px solid var(--accent); border-radius: var(--r-sm); background: var(--accent); color: var(--on-accent); padding: var(--sp-1) var(--sp-3); cursor: pointer; }
button:disabled { opacity: .5; cursor: not-allowed; }
.error { color: var(--danger); } .ok { color: var(--ok); }
</style>
