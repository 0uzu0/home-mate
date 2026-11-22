<script setup lang="ts">
/**
 * Independently editable phrases sharing the local pre-synthesis cue cache.
 */
import { ref, watch } from 'vue'
import { getMode, putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import PhraseList from '../PhraseList.vue'
import TextInput from '../TextInput.vue'
import Toggle from '../Toggle.vue'
import SectionLabel from '../SectionLabel.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

interface Draft {
  retreatWords: string[]
  retreatAck: string
  windowCloseAck: string
  truncationNotice: string
  wakeAckEnabled: boolean
  wakeAckPhrases: string[]
}

const draft = ref<Draft | null>(null)
const { busy, message, tone, run } = useSave()

watch(
  [() => props.mode, () => props.device],
  ([mode, device]) => {
    if (mode && device && draft.value === null) {
      draft.value = {
        retreatWords: [...(mode.retreat_words ?? [])],
        retreatAck: mode.retreat_ack ?? '',
        windowCloseAck: mode.window_close_ack ?? '拜拜啦',
        truncationNotice: mode.truncation_notice ?? '',
        wakeAckEnabled: device.wake_ack?.enabled !== false,
        wakeAckPhrases: [...(device.wake_ack?.phrases ?? [])],
      }
    }
  },
  { immediate: true },
)

async function save(patch: Parameters<typeof putMode>[1], detail?: string): Promise<void> {
  if (!props.device) return
  await run(async () => {
    await putMode(props.device!.device, patch)
    if (patch.retreat_ack !== undefined || patch.window_close_ack !== undefined) {
      const readback = await getMode()
      for (const key of ['retreat_ack', 'window_close_ack'] as const) {
        if (patch[key] !== undefined && readback[key] !== patch[key]) {
          throw new Error(t('phraseReadbackError'))
        }
      }
    }
    await props.refresh()
    return detail
  })
}

function saveRetreatWords(): void {
  if (!draft.value) return
  void save({ retreat_words: draft.value.retreatWords })
}

function saveRetreatAck(): void {
  if (!draft.value) return
  void save({ retreat_ack: draft.value.retreatAck.trim() })
}

function saveWindowCloseAck(): void {
  if (!draft.value) return
  void save({ window_close_ack: draft.value.windowCloseAck.trim() })
}

function saveTruncation(): void {
  if (!draft.value) return
  void save({ truncation_notice: draft.value.truncationNotice.trim() })
}

function saveWakeAck(resynth = false): void {
  if (!draft.value) return
  void save(
    {
      wake_ack: {
        enabled: draft.value.wakeAckEnabled,
        phrases: draft.value.wakeAckPhrases,
        ...(resynth ? { resynth: true } : {}),
      },
    },
    resynth ? t('resynthQueued') : undefined,
  )
}
</script>

<template>
  <Panel
    icon="phrases" apply="immediate" :title="t('phrasesTitle')" :hint="t('phrasesHint')">
    <template #actions>
      <span v-if="mode" class="phrases__current mono">
        {{ t('current') }}: {{ mode.retreat_ack || '—' }} / {{ mode.window_close_ack || '—' }} / {{ mode.truncation_notice || '—' }}
      </span>
    </template>

    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else-if="draft">
      <section class="phrases__section">
        <SectionLabel :text="t('retreatWordsTitle')" />
        <p class="phrases__hint">{{ t('retreatWordsHint') }}</p>
        <PhraseList
          v-model="draft.retreatWords"
          :label="t('retreatWordsTitle')"
          :placeholder="t('retreatPlaceholder')"
          :maxlength="8"
          :disabled="busy"
        />
        <div class="phrases__row">
          <button type="button" class="btn" :disabled="busy || !draft.retreatWords.length" @click="saveRetreatWords">
            {{ t('retreatSave') }}
          </button>
        </div>
      </section>

      <div class="phrases__acks">
        <section class="phrases__section">
          <TextInput
            v-model="draft.retreatAck"
            :label="t('retreatAckTitle')"
            :hint="t('retreatAckHint')"
            :maxlength="8"
            wide
          />
          <div class="phrases__row">
            <button type="button" class="btn" :disabled="busy || !draft.retreatAck.trim()" @click="saveRetreatAck">
              {{ t('save') }}
            </button>
          </div>
        </section>

        <section class="phrases__section">
          <TextInput
            v-model="draft.windowCloseAck"
            :label="t('windowCloseAckTitle')"
            :hint="t('windowCloseAckHint')"
            :maxlength="8"
            wide
          />
          <div class="phrases__row">
            <button type="button" class="btn" :disabled="busy || !draft.windowCloseAck.trim()" @click="saveWindowCloseAck">
              {{ t('save') }}
            </button>
          </div>
        </section>
      </div>

      <section class="phrases__section">
        <TextInput
          v-model="draft.truncationNotice"
          :label="t('truncationTitle')"
          :hint="t('truncationHint')"
          :maxlength="60"
          wide
        />
        <div class="phrases__row">
          <button type="button" class="btn" :disabled="busy" @click="saveTruncation">
            {{ t('truncationSave') }}
          </button>
        </div>
      </section>

      <section class="phrases__section">
        <Toggle v-model="draft.wakeAckEnabled" :label="t('wakeAckEnabled')" :hint="t('wakeAckHint')" />
        <PhraseList
          v-model="draft.wakeAckPhrases"
          :label="t('wakeAckTitle')"
          :placeholder="t('wakeAckPlaceholder')"
          :maxlength="6"
          :disabled="busy"
        />
        <div class="phrases__row">
          <button type="button" class="btn btn--primary" :disabled="busy" @click="saveWakeAck()">
            {{ t('phraseSave') }}
          </button>
          <button type="button" class="btn" :disabled="busy" @click="saveWakeAck(true)">
            {{ t('resynth') }}
          </button>
        </div>
      </section>

      <p v-if="message" class="phrases__msg" :class="`phrases__msg--${tone}`">{{ message }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.phrases__current {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.phrases__section {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.phrases__acks {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 16rem), 1fr));
  gap: var(--sp-3);
}

.phrases__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.phrases__row {
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
  padding: 0.22rem var(--sp-3);
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

.phrases__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.phrases__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
