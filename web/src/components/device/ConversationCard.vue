<script setup lang="ts">
/**
 * The device conversation, and the one action it needs: CLEAR.
 *
 * Why it exists (2026-11-13): the assistant replays its own earlier sentences
 * from the history. After the household configured Home Assistant, the model
 * still answered 「家庭灯光后端还没有接入」 — it was reading a transcript written
 * before the configuration. Long-term facts belong in memory; a stale
 * conversation is cleared, not argued with.
 */
import { computed, ref, watch } from 'vue'
import { ApiError, clearDeviceConversation, getDeviceConversation, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import ConfirmDialog from '../ConfirmDialog.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = defineProps<{ device: ModeDevice }>()

const selectedContext = ref('')
const archiveBefore = ref<number>()
const messageBefore = ref<number>()
const messages = useResource(
  (signal) => getDeviceConversation(props.device.device, { signal }, selectedContext.value, { archiveBefore: archiveBefore.value, messageBefore: messageBefore.value }),
  { timeoutMs: 6000, retries: 1 },
)
usePoll(() => messages.load(), 15000)

const count = computed(() => messages.data.value?.messages?.length ?? 0)
const archivedCount = computed(() => messages.data.value?.archived_contexts?.length ?? 0)
const hasArchived = computed(() => archivedCount.value > 0)
const canClear = computed(() => count.value > 0 || archivedCount.value > 0)
const countLabel = computed(() =>
  archivedCount.value > 0
    ? t('conversationCountWithArchives', { n: count.value, archives: archivedCount.value })
    : t('conversationCount', { n: count.value }),
)
const last = computed(() => {
  const rows = messages.data.value?.messages ?? []
  return rows.length ? rows[rows.length - 1] : null
})
const confirming = ref(false)
const confirmedSession = ref<string | null>(null)
const confirmedIncludeArchived = ref(false)
const { busy, message, tone, run } = useSave()
watch(() => props.device.device, () => { selectedContext.value = ''; archiveBefore.value = undefined; messageBefore.value = undefined; confirming.value = false; confirmedSession.value = null; confirmedIncludeArchived.value = false; messages.reset(); void messages.load() })

function confirmClear(): void {
  confirmedSession.value = messages.data.value?.session_id ?? null
  confirmedIncludeArchived.value = archivedCount.value > 0
  confirming.value = confirmedSession.value !== null
}

async function changeContext(event: Event): Promise<void> {
  selectedContext.value = (event.target as HTMLSelectElement).value
  messageBefore.value = undefined
  confirming.value = false
  confirmedSession.value = null
  confirmedIncludeArchived.value = false
  await messages.load()
}

async function archivePage(earlier: boolean): Promise<void> {
  archiveBefore.value = earlier ? messages.data.value?.next_archive_before ?? undefined : undefined
  await messages.load()
}
async function messagePage(earlier: boolean): Promise<void> {
  messageBefore.value = earlier ? messages.data.value?.next_message_before ?? undefined : undefined
  await messages.load()
}

async function clearConversation(): Promise<void> {
  confirming.value = false
  const id = confirmedSession.value
  const includeArchived = confirmedIncludeArchived.value
  if (!id) return
  await run(async () => {
    const r = await clearDeviceConversation(props.device.device, id, includeArchived).catch((err) => {
      if (err instanceof ApiError && err.status === 409) throw new Error(t('conversationChanged'))
      throw err
    })
    if (includeArchived) {
      archiveBefore.value = undefined
      messageBefore.value = undefined
    }
    await messages.load()
    return t('conversationCleared', { n: r.cleared })
  })
}
</script>

<template>
  <Panel icon="history" apply="immediate" :title="t('conversationTitle')" :hint="t('conversationHint')">
    <template #actions>
      <span class="mono conv__count">{{ countLabel }}</span>
    </template>

    <ErrorNotice v-if="messages.error.value" :error="messages.error.value" @retry="messages.load" />
    <div v-else class="conv">
      <p>{{ t('conversationContextBoundary') }}</p>
      <label>
        {{ t('conversationCurrentContext') }}
        <select :value="selectedContext" @change="changeContext">
          <option value="">{{ t('conversationCurrentContext') }} · {{ messages.data.value?.current_context?.generation ?? '—' }}</option>
          <option v-if="selectedContext && !(messages.data.value?.archived_contexts ?? []).some(c => c.session_id === selectedContext)" :value="selectedContext">
            {{ t('conversationArchivedContext', { n: messages.data.value?.selected_context?.generation ?? '—' }) }}
          </option>
          <option v-for="context in messages.data.value?.archived_contexts ?? []" :key="context.session_id" :value="context.session_id">
            {{ t('conversationArchivedContext', { n: context.generation }) }}
          </option>
        </select>
      </label>
      <div class="conv__actions">
        <button v-if="messages.data.value?.next_archive_before" type="button" class="btn" :disabled="messages.loading.value" @click="archivePage(true)">{{ t('conversationEarlierContexts') }}</button>
        <button v-if="archiveBefore !== undefined" type="button" class="btn" :disabled="messages.loading.value" @click="archivePage(false)">{{ t('conversationLatestContexts') }}</button>
      </div>
      <div class="conv__actions">
        <button v-if="messages.data.value?.next_message_before" type="button" class="btn" :disabled="messages.loading.value" @click="messagePage(true)">{{ t('conversationEarlierMessages') }}</button>
        <button v-if="messageBefore !== undefined" type="button" class="btn" :disabled="messages.loading.value" @click="messagePage(false)">{{ t('conversationLatestMessages') }}</button>
      </div>
      <details v-if="count">
        <summary>{{ t('conversationCount', { n: count }) }}</summary>
        <p v-for="(row, index) in messages.data.value?.messages ?? []" :key="index"><span class="mono conv__role">{{ row.role }}</span> {{ row.content }}</p>
      </details>
      <p class="conv__last">
        <template v-if="last">
          <span class="mono conv__role">{{ last.role }}</span>
          <span class="conv__text">{{ last.content }}</span>
        </template>
        <template v-else>{{ t('conversationEmpty') }}</template>
      </p>
      <div class="conv__actions">
        <button
          type="button"
          class="btn btn--danger"
          :disabled="Boolean(selectedContext) || busy || messages.loading.value || Boolean(messages.error.value) || !messages.data.value?.session_id || !canClear"
          @click="confirmClear"
        >
          {{ t('conversationClear') }}
        </button>
        <span v-if="message" class="conv__msg" :class="`conv__msg--${tone}`">{{ message }}</span>
      </div>
    </div>

    <ConfirmDialog
      v-if="confirming"
      :title="t('conversationClear')"
      :message="hasArchived ? t('conversationClearAllConfirm', { archives: archivedCount }) : t('conversationClearConfirm')"
      :confirm-label="t('conversationClear')"
      :cancel-label="t('cancel')"
      tone="danger"
      @confirm="clearConversation"
      @cancel="confirming = false"
    />
  </Panel>
</template>

<style scoped>
.conv {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.conv__count {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.conv__last {
  margin: 0;
  font-size: var(--fs-sm);
  color: var(--muted);
  display: flex;
  gap: var(--sp-2);
  min-height: 1.2em;
}

.conv__role {
  color: var(--ink);
  flex: none;
}

.conv__text {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.conv__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
}

.conv__msg {
  font-size: var(--fs-sm);
}

.conv__msg--ok {
  color: var(--ok);
}

.conv__msg--danger {
  color: var(--danger);
}
</style>
