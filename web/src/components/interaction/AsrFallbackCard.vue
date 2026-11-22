<script setup lang="ts">
/**
 * ASR wake fallback (user decision 2026-11-14, commercial privacy).
 *
 * The question this card answers is the one a buyer asks: does the speaker
 * transcribe everything said in the room? The honest answer is "yes, unless
 * you turn this off" — the fallback transcribes every closed voice segment
 * so a missed keyword can still wake the assistant. Off = ASR runs only
 * inside an open session; idle room speech never reaches the ASR endpoint.
 *
 * The switch is GLOBAL (it is a capture policy, not a device preference),
 * so the card says so and the counter is the selected device's own proof.
 */
import { ref, watch } from 'vue'
import { putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Toggle from '../Toggle.vue'
import StatTile from '../StatTile.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const enabled = ref<boolean | null>(null)
/** The switch follows the SERVER until the user touches it: another client
 * changing the setting used to leave this draft showing the old value, and a
 * save would then overwrite the newer one (audit 2026-11-14). */
const touched = ref(false)
const { busy, message, tone, run } = useSave()

watch(
  () => props.mode,
  (mode) => {
    if (!mode) return
    if (!touched.value) enabled.value = mode.asr_wake_fallback !== false
  },
  { immediate: true },
)

function onToggle(v: boolean): void {
  touched.value = true
  enabled.value = v
}

async function save(): Promise<void> {
  if (!props.device || enabled.value === null) return
  await run(async () => {
    await putMode(props.device!.device, { asr_wake_fallback: enabled.value! })
    touched.value = false
    await props.refresh()
  })
}
</script>

<template>
  <Panel
    icon="wake"
    apply="immediate"
    :title="t('asrFallbackTitle')"
    :hint="t('asrFallbackHint')"
  >
    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <Toggle
        v-if="enabled !== null"
        :model-value="enabled"
        :label="t('asrFallbackEnabled')"
        @update:model-value="onToggle"
      />

      <p class="asr__note" :class="enabled ? 'asr__note--warn' : 'asr__note--ok'">
        {{ enabled ? t('asrFallbackOnNote') : t('asrFallbackOffNote', { seconds: (mode?.follow_window_ms ?? 20000) / 1000 }) }}
      </p>

      <p class="asr__global">{{ t('asrFallbackGlobal') }}</p>

      <div class="asr__actions">
        <button type="button" class="btn btn--primary" :disabled="busy || enabled === null" @click="save">
          {{ busy ? t('saving') : t('asrFallbackSave') }}
        </button>
        <span v-if="message" class="asr__msg" :class="`asr__msg--${tone}`">{{ message }}</span>
      </div>

      <div class="asr__grid">
        <StatTile
          :label="t('asrSkipped')"
          :value="device.mic?.counters?.asr_skipped ?? 0"
          tone="muted"
          :note="t('asrSkippedHint')"
        />
        <StatTile
          :label="t('asrIgnored')"
          :value="device.mic?.counters?.ignored_unwoken ?? 0"
          tone="muted"
          :note="t('asrIgnoredHint')"
        />
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.asr__note {
  font-size: var(--fs-sm);
  line-height: 1.5;
  border-left: 3px solid var(--line-strong);
  padding-left: var(--sp-2);
}

.asr__note--warn {
  border-left-color: var(--warn);
}

.asr__note--ok {
  border-left-color: var(--ok);
}

.asr__global {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.asr__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  gap: var(--sp-2);
}

.asr__actions {
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

.asr__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.asr__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
