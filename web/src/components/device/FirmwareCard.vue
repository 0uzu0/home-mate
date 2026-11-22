<script setup lang="ts">
/**
 * Firmware and core A/B slots: the real slot state, and the upload that writes
 * the inactive slot. The restart checkbox is audio-firmware only; the core
 * update always restarts (that is how the slot is flipped).
 */
import { computed, ref } from 'vue'
import {
  getFirmware,
  updateFirmwareAudio,
  updateFirmwareCore,
  type FirmwareStatus,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail, formatBytes } from '../../format'
import { useResource } from '../../hooks/useResource'
import Panel from '../Panel.vue'
import ConfirmDialog from '../ConfirmDialog.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'

const firmware = useResource((signal) => getFirmware({ signal }), { timeoutMs: 6000, retries: 1 })
void firmware.load()

const restart = ref(true)
const busyKind = ref<'audio' | 'core' | ''>('')
const message = ref('')
const tone = ref<'ok' | 'error' | ''>('')

const layouts = computed<Array<{ kind: 'audio' | 'core'; title: string; layout: FirmwareStatus['audio'] }>>(
  () => {
    const data = firmware.data.value
    if (!data) return []
    return [
      { kind: 'audio', title: t('fwAudio'), layout: data.audio },
      { kind: 'core', title: t('fwCore'), layout: data.core },
    ]
  },
)

/** Selecting a file used to start replacing the running component immediately.
 * Now the file is staged and the user confirms what it is and what happens
 * (the inactive slot is written, the pointer flips, the component restarts). */
const staged = ref<{ file: File; kind: 'audio' | 'core' } | null>(null)
const confirming = ref(false)

async function upload(event: Event, kind: 'audio' | 'core'): Promise<void> {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  if (!file) {
    message.value = t('fwNoFile')
    tone.value = 'error'
    return
  }
  staged.value = { file, kind }
  confirming.value = true
}

async function confirmUpload(): Promise<void> {
  const target = staged.value
  confirming.value = false
  staged.value = null
  if (!target) return
  const { file, kind } = target
  busyKind.value = kind
  message.value = ''
  tone.value = ''
  try {
    const result =
      kind === 'audio'
        ? await updateFirmwareAudio(file, restart.value)
        : await updateFirmwareCore(file)
    message.value = t('fwStaged', {
      slot: result.staged_slot,
      version: result.version ?? '—',
      size: formatBytes(result.size),
    })
    if (kind === 'audio' && result.restarted) message.value += ` · ${t('fwRestarted')}`
    if (kind === 'core') message.value += ` · ${t('fwCoreRestarting')}`
    tone.value = 'ok'
    await firmware.load()
  } catch (err) {
    message.value = errorDetail(err)
    tone.value = 'error'
  } finally {
    busyKind.value = ''
  }
}
</script>

<template>
  <Panel
    icon="firmware" apply="restart" :title="t('fwTitle')" :hint="t('fwHint')">
    <template #actions>
      <button type="button" class="btn" :disabled="firmware.loading.value" @click="firmware.load()">
        <AppIcon name="reload" :size="13" /> {{ t('refresh') }}
      </button>
    </template>

    <p v-if="firmware.error.value && !firmware.data.value" class="fw__hint">
      {{ errorDetail(firmware.error.value) }}
    </p>

    <!-- Audit R2-4: two update entry points used to be peers. The release
         package is the daily path (Manifest signature, health check, rollback);
         a single file is maintenance. Say so, and link to the real one. -->
    <p class="fw__daily">
      {{ t('fwDailyViaRelease') }}
      <RouterLink to="/releases">{{ t('fwOpenReleases') }}</RouterLink>
    </p>

    <div v-for="item in layouts" :key="item.kind" class="fw">
      <div class="fw__head">
        <p class="fw__title">{{ item.title }}</p>
        <Pill v-if="item.layout.active_slot" size="sm" mono>
          {{ t('fwActive') }}: {{ item.layout.active_slot }}
        </Pill>
      </div>

      <p v-if="!item.layout.available" class="fw__hint">{{ t('fwUnavailable') }}</p>

      <template v-else>
        <div class="fw__slots">
          <div class="fw__slot-head">
            <span>{{ t('fwSlot') }}</span>
            <span>{{ t('fwVersion') }}</span>
            <span>{{ t('fwSize') }}</span>
            <span>sha8</span>
          </div>
          <div v-for="slot in item.layout.slots" :key="slot.name" class="fw__slot">
            <span class="mono">
              {{ slot.name }}
              <Pill v-if="slot.name === item.layout.active_slot" tone="ok" size="sm">
                <AppIcon name="check" :size="11" />
              </Pill>
            </span>
            <span class="mono">{{ slot.version ?? '—' }}</span>
            <span class="mono">{{ formatBytes(slot.size) }}</span>
            <span class="mono">{{ slot.sha8 }}</span>
          </div>
        </div>

        <details class="fw__advanced">
          <summary>{{ t('fwAdvanced') }}</summary>
          <div class="fw__row">
            <Toggle v-if="item.kind === 'audio'" v-model="restart" :label="t('fwRestartAfter')" />
            <label class="fw__file">
              <span class="btn"><AppIcon name="upload" :size="13" /> {{ busyKind === item.kind ? t('testing') : t('fwUpload') }}</span>
              <input
                type="file"
                :disabled="busyKind !== ''"
                @change="upload($event, item.kind)"
              />
            </label>
          </div>
          <p class="fw__hint">{{ item.kind === 'audio' ? t('fwAudioHint') : t('fwCoreHint') }}</p>
        </details>
      </template>
    </div>

    <p v-if="message" class="fw__msg" :class="`fw__msg--${tone}`">{{ message }}</p>

    <ConfirmDialog
      v-if="confirming && staged"
      :title="t('fwConfirmTitle')"
      :message="t('fwConfirmMessage', { file: staged.file.name, size: formatBytes(staged.file.size), target: staged.kind === 'core' ? t('fwCore') : t('fwAudio') })"
      :confirm-label="t('fwUpload')"
      :cancel-label="t('cancel')"
      tone="danger"
      :busy="busyKind !== ''"
      @confirm="confirmUpload"
      @cancel="staged = null; confirming = false"
    />
  </Panel>
</template>

<style scoped>
.fw {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.fw__head {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.fw__daily {
  font-size: var(--fs-sm);
  color: var(--muted);
  max-width: 75ch;
}

.fw__daily a {
  color: var(--accent);
  white-space: nowrap;
}

.fw__advanced {
  border-top: 1px dashed var(--line);
  padding-top: var(--sp-1);
}

.fw__advanced summary {
  cursor: pointer;
  color: var(--muted);
  font-size: var(--fs-xs);
}

.fw__advanced summary:hover {
  color: var(--ink);
}

.fw__title {
  font-size: var(--fs-sm);
  font-weight: 650;
}

.fw__slots {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  overflow-x: auto;
}

.fw__slot-head,
.fw__slot {
  display: grid;
  grid-template-columns: 8rem 7rem 7rem 7rem;
  gap: var(--sp-2);
  align-items: center;
  min-width: 30rem;
  font-size: var(--fs-xs);
}

.fw__slot-head {
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: var(--tracking-label);
  border-bottom: 1px solid var(--line);
  padding-bottom: var(--sp-1);
}

.fw__slot {
  padding: var(--sp-1) 0;
  border-bottom: 1px solid var(--line);
}

.fw__row {
  display: flex;
  align-items: center;
  gap: var(--sp-4);
  flex-wrap: wrap;
}

.fw__file {
  position: relative;
  overflow: hidden;
  display: inline-flex;
}

.fw__file input {
  position: absolute;
  inset: 0;
  opacity: 0;
  cursor: pointer;
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.22rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
  display: inline-block;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.fw__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.fw__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.fw__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
