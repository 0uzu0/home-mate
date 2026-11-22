<script setup lang="ts">
/**
 * Panel 2: Continuous Conversation / Follow-up Window (连续对话（应答窗）)
 *
 * Unifies follow-up window toggle, duration, turn budget, close-tone toggle,
 * end/retreat words & goodbye phrases, live window state + manual close, and
 * clearing the current device conversation.
 */
import { computed, ref, watch } from 'vue'
import {
  ApiError,
  clearDeviceConversation,
  closeSession,
  getMode,
  getDeviceConversation,
  putMode,
  type ModeDevice,
  type ModeSnapshot,
} from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import NumberField from '../NumberField.vue'
import PhraseList from '../PhraseList.vue'
import TextInput from '../TextInput.vue'
import ConfirmDialog from '../ConfirmDialog.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const enabled = ref(true)
const seconds = ref(9)
const contextSeconds = ref(240)
const closeTone = ref(false)
const retreatWords = ref<string[]>([])
const retreatAck = ref('')
const windowCloseAck = ref('拜拜啦')
const touched = ref(false)

const confirmingClear = ref(false)
const { busy, message, tone, run } = useSave()
const {
  busy: sessionBusy,
  message: sessionMsg,
  tone: sessionTone,
  run: runSession,
} = useSave()

const messages = useResource(
  (signal) =>
    props.device
      ? getDeviceConversation(props.device.device, { signal })
      : Promise.resolve({ session_id: null, messages: [] }),
  { timeoutMs: 6000, retries: 1 },
)
usePoll(() => {
  if (props.device) return messages.load()
}, 15000)
const confirmedSession = ref<string | null>(null)
watch(() => props.device?.device, () => { confirmingClear.value = false; confirmedSession.value = null; messages.reset(); void messages.load() })

function confirmClear(): void {
  confirmedSession.value = messages.data.value?.session_id ?? null
  confirmingClear.value = confirmedSession.value !== null
}

const msgCount = computed(() => messages.data.value?.messages?.length ?? 0)

watch(
  () => props.mode,
  (mode) => {
    if (!mode || touched.value) return
    enabled.value = mode.follow_window_enabled ?? true
    seconds.value = (mode.follow_window_ms ?? 9000) / 1000
    contextSeconds.value = (mode.context_window_ms ?? 240000) / 1000
    closeTone.value = mode.follow_window_tone ?? false
    retreatWords.value = [...(mode.retreat_words ?? [])]
    retreatAck.value = mode.retreat_ack ?? ''
    windowCloseAck.value = mode.window_close_ack ?? '拜拜啦'
  },
  { immediate: true },
)

const session = computed(() => props.device?.session ?? null)
const phaseLabel = computed(() => {
  switch (session.value?.phase) {
    case 'closed': return t('sessionIdle')
    case 'active': return t('sessionActive')
    case 'tail': return t('sessionTail')
    case 'closing': return t('sessionClosingPhase')
    default: return '—'
  }
})

const phaseTone = computed<'ok' | 'info' | 'muted'>(() => {
  if (session.value?.phase === 'active') return 'ok'
  if (session.value?.phase === 'tail') return 'info'
  return 'muted'
})

async function save(): Promise<void> {
  if (!props.device) return
  const ack = windowCloseAck.value.trim()
  const patch = {
    follow_window_enabled: enabled.value,
    follow_window_ms: seconds.value * 1000,
    context_window_ms: contextSeconds.value * 1000,
    follow_window_tone: closeTone.value,
    retreat_words: retreatWords.value,
    retreat_ack: retreatAck.value.trim(),
    ...(closeTone.value || ack ? { window_close_ack: ack } : {}),
  }
  await run(async () => {
    await putMode(props.device!.device, patch)
    const readback = await getMode()
    if (
      readback.follow_window_enabled !== patch.follow_window_enabled ||
      readback.follow_window_ms !== patch.follow_window_ms ||
      readback.context_window_ms !== patch.context_window_ms ||
      readback.follow_window_tone !== patch.follow_window_tone
    ) {
      throw new Error(t('followWindowReadbackError'))
    }
    touched.value = false
    await props.refresh()
  })
}

async function handleCloseWindow(): Promise<void> {
  if (!props.device) return
  await runSession(async () => {
    try {
      await closeSession(props.device!.device, session.value?.generation)
    } finally {
      await props.refresh()
    }
    return t('sessionCloseHint')
  })
}

async function handleClearConversation(): Promise<void> {
  confirmingClear.value = false
  const id = confirmedSession.value
  if (!id || !props.device) return
  await runSession(async () => {
    const r = await clearDeviceConversation(props.device!.device, id).catch((err) => {
      if (err instanceof ApiError && err.status === 409) throw new Error(t('conversationChanged'))
      throw err
    })
    await messages.load()
    return t('conversationCleared', { n: r.cleared })
  })
}
</script>

<template>
  <Panel icon="wake" apply="immediate" :title="t('panelFollowTitle')" :hint="t('panelFollowHint')">
    <template #actions>
      <Pill v-if="session" size="sm" :tone="phaseTone">{{ phaseLabel }}</Pill>
    </template>

    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <div class="fp__block">
        <Toggle
          v-model="enabled"
          :label="t('followWindowEnabled')"
          :disabled="busy"
          @update:model-value="touched = true"
        />
        <div class="fp__row">
          <NumberField
            v-model="seconds"
            :label="t('followWindowSeconds')"
            :min="5"
            :max="60"
            :disabled="busy"
            @update:model-value="touched = true"
          />
          <NumberField
            v-model="contextSeconds"
            :label="t('contextWindowSeconds')"
            :min="60"
            :max="600"
            :disabled="busy"
            @update:model-value="touched = true"
          />
        </div>
        <p class="fp__hint">{{ t('followWindowLifecycleHint') }}</p>
        <Toggle
          v-model="closeTone"
          :label="t('followTone')"
          :hint="t('followToneHint')"
          :disabled="busy"
          @update:model-value="touched = true"
        />
      </div>

      <div class="fp__block">
        <PhraseList
          v-model="retreatWords"
          :label="t('retreatWordsTitle')"
          :placeholder="t('retreatPlaceholder')"
          :maxlength="8"
          :disabled="busy"
          @update:model-value="touched = true"
        />
        <p class="fp__hint">{{ t('retreatWordsHint') }}</p>
        <div class="fp__acks">
          <TextInput
            v-model="retreatAck"
            :label="t('retreatAckTitle')"
            :hint="t('retreatAckHint')"
            :maxlength="8"
            wide
            @update:model-value="touched = true"
          />
          <TextInput
            v-model="windowCloseAck"
            :label="t('windowCloseAckTitle')"
            :hint="t('windowCloseAckHint')"
            :maxlength="8"
            wide
            @update:model-value="touched = true"
          />
        </div>
      </div>

      <div class="fp__actions">
        <button
          type="button"
          class="btn btn--primary"
          :disabled="busy || !retreatWords.length || !retreatAck.trim() || (closeTone && !windowCloseAck.trim())"
          @click="save"
        >
          {{ busy ? t('saving') : t('followWindowSave') }}
        </button>
        <span v-if="message" class="fp__msg" :class="`fp__msg--${tone}`" role="status">{{ message }}</span>
      </div>

      <div class="fp__live">
        <ErrorNotice v-if="messages.error.value" :error="messages.error.value" @retry="messages.load" />
        <dl class="fp__dl" aria-live="polite">
          <dt>{{ t('sessionPhase') }}</dt>
          <dd>{{ phaseLabel }}</dd>
          <dt>{{ t('sessionCredits') }}</dt>
          <dd>{{ session?.credits ?? '—' }}</dd>
          <dt>{{ t('sessionDeadline') }}</dt>
          <dd>
            {{
              session?.tail_deadline_ms
                ? formatTime(session.tail_deadline_ms)
                : session?.phase === 'active'
                  ? t('sessionRunning')
                  : '—'
            }}
          </dd>
          <dt>{{ t('conversationTitle') }}</dt>
          <dd class="mono">{{ t('conversationCount', { n: msgCount }) }}</dd>
        </dl>
        <div class="fp__actions">
          <button type="button" class="btn" :disabled="sessionBusy || !session" @click="handleCloseWindow">
            {{ sessionBusy ? t('saving') : t('sessionClose') }}
          </button>
          <button
            type="button"
            class="btn btn--danger"
            :disabled="sessionBusy || messages.loading.value || Boolean(messages.error.value) || !messages.data.value?.session_id || msgCount === 0"
            @click="confirmClear"
          >
            {{ t('conversationClear') }}
          </button>
          <span v-if="sessionMsg" class="fp__msg" :class="`fp__msg--${sessionTone}`" role="status">
            {{ sessionMsg }}
          </span>
        </div>
      </div>
    </template>

    <ConfirmDialog
      v-if="confirmingClear"
      :title="t('conversationClear')"
      :message="t('conversationClearConfirm')"
      :confirm-label="t('conversationClear')"
      :cancel-label="t('cancel')"
      tone="danger"
      @confirm="handleClearConversation"
      @cancel="confirmingClear = false"
    />
  </Panel>
</template>

<style scoped>
.fp__block,
.fp__live {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.fp__row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-3);
  align-items: end;
}

.fp__acks {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 15rem), 1fr));
  gap: var(--sp-3);
}

.fp__hint {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.fp__dl {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: var(--sp-1) var(--sp-4);
  font-size: var(--fs-sm);
}

.fp__dl dt {
  color: var(--muted);
}

.fp__dl dd {
  margin: 0;
}

.fp__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.fp__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.fp__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
