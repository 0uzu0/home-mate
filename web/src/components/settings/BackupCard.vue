<script setup lang="ts">
/**
 * Config backup: one JSON file, secrets opt-in, restore through the same
 * validation and change record as every other save. The import is confirmed
 * because it overwrites the current configuration.
 */
import { ref, watch } from 'vue'
import { getConfigExport, importConfig } from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useSave } from '../../hooks/useSave'
import { advancedMode } from '../../stores/advanced'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import Toggle from '../Toggle.vue'
import ConfirmDialog from '../ConfirmDialog.vue'

const withSecrets = ref(false)
watch(advancedMode, (on) => {
  if (!on) withSecrets.value = false
})
const { busy, message, tone, run } = useSave()
const pendingDoc = ref<unknown | null>(null)
const pendingName = ref('')

async function exportBackup(): Promise<void> {
  await run(async () => {
    const doc = await getConfigExport(advancedMode.value && withSecrets.value)
    const blob = new Blob([JSON.stringify(doc, null, 1)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `hm-config-${new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)}.json`
    anchor.click()
    URL.revokeObjectURL(url)
    return t('backupExported')
  })
}

async function pickFile(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  if (!file) return
  try {
    pendingDoc.value = JSON.parse(await file.text())
    pendingName.value = file.name
  } catch (err) {
    message.value = errorDetail(err)
    tone.value = 'error'
  }
}

async function confirmImport(): Promise<void> {
  if (pendingDoc.value === null) return
  await run(async () => {
    const result = await importConfig(pendingDoc.value)
    // Warnings are part of the outcome (e.g. the backup carried secrets but
    // this install has no keys, so the existing ones were kept): showing only
    // "imported N" hid them (audit 2026-11-14).
    const base = t('backupImported', { n: result.applied.length })
    const warnings = result.warnings ?? []
    return warnings.length ? `${base} — ${warnings.join('; ')}` : base
  })
  pendingDoc.value = null
  pendingName.value = ''
}
</script>

<template>
  <Panel
    icon="backup" :title="t('backupTitle')" :hint="t('backupHint')">
    <Toggle v-if="advancedMode" v-model="withSecrets" :label="t('backupWithSecrets')" />

    <div class="backup__row">
      <button type="button" class="btn btn--primary" :disabled="busy" @click="exportBackup">
        <AppIcon name="download" :size="13" /> {{ busy ? t('saving') : t('backupExport') }}
      </button>
      <label class="backup__file">
        <span class="btn"><AppIcon name="upload" :size="13" /> {{ t('backupImport') }}</span>
        <input type="file" accept="application/json,.json" :disabled="busy" @change="pickFile" />
      </label>
      <span class="backup__hint">{{ t('backupFileHint') }}</span>
    </div>

    <p v-if="message" class="backup__msg" :class="`backup__msg--${tone}`">{{ message }}</p>

    <ConfirmDialog
      v-if="pendingDoc !== null"
      :title="t('backupConfirmTitle')"
      :message="`${t('backupConfirmBody')}${pendingName ? ` (${pendingName})` : ''}`"
      :confirm-label="t('confirm')"
      :cancel-label="t('cancel')"
      :busy="busy"
      :busy-label="t('saving')"
      tone="danger"
      @confirm="confirmImport"
      @cancel="pendingDoc = null"
    />
  </Panel>
</template>

<style scoped>
.backup__row {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.backup__file {
  position: relative;
  overflow: hidden;
  display: inline-flex;
}

.backup__file input {
  position: absolute;
  inset: 0;
  opacity: 0;
  cursor: pointer;
}

.backup__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.24rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
  display: inline-block;
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

.backup__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.backup__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
