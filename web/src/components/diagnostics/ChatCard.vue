<script setup lang="ts">
/**
 * Text chat: the debug entry into the same brain path, without the microphone
 * or voice playback. Streamed tokens, tool receipts and cancellation are all
 * shown; a trace id links to the Traces card on this page.
 */
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { ApiError, cancelTask, createChatScope, getChatScope, revokeChatScope, postChat, type ChatScopeBinding } from '../../api'
import { authToken } from '../../auth'
import { RouterLink } from 'vue-router'
import type { ConfirmationAction } from '../../api'
import { confirmationActions, receiveConfirmation, refreshConfirmation } from '../../stores/confirmations'
import { t, type Key } from '../../i18n'
import { errorDetail } from '../../format'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'
import TextInput from '../TextInput.vue'
import Select from '../Select.vue'
import Toggle from '../Toggle.vue'

const emit = defineEmits<{ 'open-trace': [id: string] }>()

interface ToolChip {
  tool: string
  summary: string
  status: string
}

interface Bubble {
  role: 'user' | 'assistant'
  text: string
  tools: ToolChip[]
  cancelled: boolean
  traceId: string | null
  confirmationId: string | null
  pendingTaskId: string | null
}

const bubbles = ref<Bubble[]>([])
const input = ref('')
const running = ref(false)
const taskId = ref<string | null>(null)
const listEl = ref<HTMLElement | null>(null)
const controller = ref<AbortController | null>(null)

function readStoredSession(): string | null {
  try {
    return localStorage.getItem('hm.session')
  } catch {
    return null
  }
}

function writeStoredSession(id: string | null): void {
  try {
    if (id) localStorage.setItem('hm.session', id)
    else localStorage.removeItem('hm.session')
  } catch {
    /* private mode / storage disabled */
  }
}

const sessionId = ref<string | null>(readStoredSession())
const sendError = ref('')
const scopeError = ref('')
const scopeBusy = ref(false)
const scopeInvalid = ref(false)
const scopeMode = ref('headless')
const person = ref('')
const device = ref('')
const householdWrite = ref(false)
const binding = ref<ChatScopeBinding | null>(null)
const pending = ref<{ binding: ChatScopeBinding; selection: string } | null>(null)
const scopeController = new AbortController()
const now = ref(Date.now())
let clock: number | null = null
let unmounted = false
const scopeOptions = computed(() => [
  { value: 'headless', label: t('chatScopeHeadless') },
  { value: 'household', label: t('chatScopeHousehold') },
  { value: 'person', label: t('chatScopePerson') },
  { value: 'device', label: t('chatScopeDevice') },
  { value: 'person-device', label: t('chatScopePersonDevice') },
])
const needsPerson = computed(() => scopeMode.value.includes('person'))
const needsDevice = computed(() => scopeMode.value.includes('device'))
const desiredScope = computed(() => ({
  ...(needsPerson.value ? { person_id: person.value.trim() } : {}),
  ...(needsDevice.value ? { device_id: device.value.trim() } : {}),
  household_write: scopeMode.value === 'household' && householdWrite.value,
}))
const selection = computed(() => JSON.stringify([scopeMode.value, desiredScope.value]))
const appliedSelection = ref(selection.value)
const scopeDirty = computed(() => selection.value !== appliedSelection.value)
const expired = computed(() => binding.value !== null && now.value >= binding.value.expires_ms)
const scopeBlocked = computed(() => scopeBusy.value || scopeDirty.value || pending.value !== null || scopeInvalid.value || expired.value)
const canSend = computed(() => input.value.trim() && !running.value && !scopeBlocked.value)
const scopeSummary = computed(() => {
  const s = binding.value?.scope
  return s ? [s.person_id ? `${t('chatScopePersonId')}: ${s.person_id}` : '',
    s.device_id ? `${t('chatScopeDeviceId')}: ${s.device_id}` : ''].filter(Boolean).join(' · ') || t('chatScopeHousehold')
    : t('chatScopeHeadless')
})

function detail(err: unknown): string {
  const raw = errorDetail(err)
  if (err instanceof ApiError && err.status === 401) return t('chatScopePairing')
  if (raw.includes('SESSION_SCOPE_UNVERIFIED') || raw.includes('SESSION_SCOPE_MISMATCH')) return t('chatScopeLegacy')
  if (err instanceof ApiError && err.status === 409) return t('chatScopeUnavailable')
  return raw
}

function resetConversation(): void {
  sessionId.value = null
  writeStoredSession(null)
  bubbles.value = []
  sendError.value = ''
}

async function clearScope(): Promise<void> {
  if (running.value || scopeBusy.value) return
  scopeBusy.value = true
  scopeError.value = ''
  try {
    // Until every revocation succeeds, sending remains blocked. A failed
    // revocation never silently turns a scoped conversation into headless.
    scopeInvalid.value = Boolean(binding.value || pending.value)
    for (const id of new Set([binding.value?.binding_id, pending.value?.binding.binding_id].filter((id): id is string => Boolean(id)))) {
      await revokeSelection(id)
    }
    if (unmounted) return
    binding.value = null; pending.value = null; scopeInvalid.value = false
    scopeMode.value = 'headless'; person.value = ''; device.value = ''; householdWrite.value = false
    appliedSelection.value = selection.value
    resetConversation()
  } catch (err) { if (!unmounted) scopeError.value = detail(err) }
  finally { scopeBusy.value = false }
}

async function revokeSelection(id: string): Promise<void> {
  try { await revokeChatScope(id, { signal: scopeController.signal }) }
  catch (err) {
    // Explicit removal/reselection can discard a server-confirmed unavailable
    // reference after restart/expiry. Network/5xx failure is not that evidence.
    if (!(err instanceof ApiError && err.status === 409)) throw err
  }
}

async function applyScope(force = false): Promise<void> {
  if (running.value || scopeBusy.value) return
  if (scopeMode.value === 'headless') { await clearScope(); return }
  if (!authToken.value) { scopeError.value = t('chatScopePairing'); return }
  if ((needsPerson.value && !person.value.trim()) || (needsDevice.value && !device.value.trim())) {
    scopeError.value = t('chatScopeRequired'); return
  }
  scopeBusy.value = true; scopeError.value = ''
  try {
    if (pending.value && (pending.value.selection !== selection.value || force)) {
      await revokeSelection(pending.value.binding.binding_id)
      pending.value = null
    }
    if (!pending.value || force) {
      const issued = await createChatScope(desiredScope.value, { signal: scopeController.signal })
      pending.value = { binding: issued, selection: selection.value }
    }
    const issued = pending.value.binding
    const current = await getChatScope(issued.binding_id, { signal: scopeController.signal })
    if (current.binding_id !== issued.binding_id || current.session_id !== issued.session_id
        || (current.scope.person_id ?? '') !== (desiredScope.value.person_id ?? '')
        || (current.scope.device_id ?? '') !== (desiredScope.value.device_id ?? '')
        || current.household_write !== desiredScope.value.household_write || current.expires_ms <= Date.now()) {
      throw new Error(t('chatScopeReadbackMismatch'))
    }
    if (binding.value && binding.value.binding_id !== current.binding_id) {
      await revokeSelection(binding.value.binding_id)
    }
    if (unmounted) return
    resetConversation()
    binding.value = current; sessionId.value = current.session_id
    appliedSelection.value = selection.value
    pending.value = null; scopeInvalid.value = false; now.value = Date.now()
  } catch (err) { if (!unmounted) scopeError.value = detail(err) }
  finally { scopeBusy.value = false }
}

function scrollBottom(): void {
  void nextTick(() => {
    const el = listEl.value
    if (el) el.scrollTop = el.scrollHeight
  })
}

async function send(): Promise<void> {
  const text = input.value.trim()
  if (!text || !canSend.value) return
  sendError.value = ''
  const start = bubbles.value.length
  let accepted = false
  bubbles.value.push({ role: 'user', text, tools: [], cancelled: false, traceId: null, confirmationId: null, pendingTaskId: null })
  // A reactive object: the stream mutates it after it is in the list, and a
  // raw object would never notify the view.
  const reply = reactive<Bubble>({
    role: 'assistant',
    text: '',
    tools: [],
    cancelled: false,
    traceId: null,
    confirmationId: null,
    pendingTaskId: null,
  })
  bubbles.value.push(reply)
  running.value = true
  scrollBottom()

  const abort = new AbortController()
  controller.value = abort
  try {
    await postChat(
      sessionId.value,
      text,
      (event) => {
        switch (event.type) {
          case 'session':
            accepted = true
            input.value = ''
            sessionId.value = String(event.session_id)
            if (!binding.value) writeStoredSession(sessionId.value)
            break
          case 'task':
            taskId.value = String(event.task_id)
            break
          case 'token':
          // R2: the core's own sentence (an action fact or a refusal). It is
          // part of the answer the person hears, so the transcript shows it.
          case 'fact':
            reply.text += String(event.text)
            scrollBottom()
            break
          case 'tool_start':
            reply.tools.push({ tool: String(event.tool), summary: '…', status: 'running' })
            break
          case 'tool_result': {
            const last = reply.tools[reply.tools.length - 1]
            if (last && last.status === 'running') {
              last.summary = String(event.summary)
              last.status = String(event.status)
            } else {
              reply.tools.push({
                tool: String(event.tool),
                summary: String(event.summary),
                status: String(event.status),
              })
            }
            break
          }
          case 'confirmation_required': {
            const action = event.action as ConfirmationAction
            if (action?.id) {
              receiveConfirmation(action)
              reply.confirmationId = action.id
              reply.pendingTaskId = action.task_id
            }
            break
          }
          case 'cancelled':
            reply.cancelled = true
            break
          case 'turn_end':
            reply.traceId = String(event.trace_id)
            if (event.status === 'failed' && !reply.text.trim()) {
              reply.text += `\n⚠ ${t('chatTurnFailed')}`
            }
            break
          case 'error':
            if (binding.value && /scope binding|scope principal|execution data epoch/i.test(String(event.message))) {
              scopeInvalid.value = true
              input.value = text
              sendError.value = t('chatScopeUnavailable')
            }
            reply.text += `\n⚠ ${String(event.message)}`
            break
        }
      },
      { signal: abort.signal, scopeBindingId: binding.value?.binding_id },
    )
  } catch (err) {
    if (!abort.signal.aborted) {
      if (!accepted) { bubbles.value.splice(start); sendError.value = detail(err) }
      else reply.text += `\n⚠ ${detail(err)}`
      if (binding.value && err instanceof ApiError && (err.status === 401 || err.status === 409)) scopeInvalid.value = true
    }
  } finally {
    running.value = false
    taskId.value = null
    controller.value = null
    scrollBottom()
  }
}

async function stop(): Promise<void> {
  const id = taskId.value
  const activeController = controller.value
  const last = bubbles.value[bubbles.value.length - 1]
  if (last?.role === 'assistant') last.cancelled = true
  try {
    if (id) await cancelTask(id)
  } catch (err) {
    sendError.value = detail(err)
  } finally {
    activeController?.abort()
  }
}

async function cancelPending(bubble: Bubble): Promise<void> {
  if (!bubble.pendingTaskId || !bubble.confirmationId) return
  try {
    await cancelTask(bubble.pendingTaskId)
    await refreshConfirmation(bubble.confirmationId)
  } catch (err) { sendError.value = detail(err) }
}

function confirmationSummary(bubble: Bubble): string {
  const action = bubble.confirmationId ? confirmationActions.get(bubble.confirmationId) : null
  const labels: Record<string, Key> = { consumed: 'confirmationConsumed', rejected: 'confirmationRejected',
    expired: 'confirmationExpired', cancelled: 'confirmationCancelled', stale: 'confirmationStale' }
  if (action?.outcome?.status === 'unknown') return t('confirmationUnknown')
  if (action?.state === 'consumed' && action.outcome?.status === 'completed') return t(
    action.outcome.fact?.confirmed && action.outcome.evidence === 'device_confirmed' ? 'confirmationConfirmed' : 'confirmationUnconfirmed')
  if (action?.state === 'consumed' && action.outcome?.status === 'failed') return t('confirmationFailed')
  if (action?.state === 'consumed' && action.outcome?.status === 'partial') return t('confirmationPartial')
  return action && labels[action.state] ? t(labels[action.state]!) : t('chatConfirmationRequired')
}

function pendingAction(bubble: Bubble): boolean {
  const state = bubble.confirmationId ? confirmationActions.get(bubble.confirmationId)?.state : null
  return state === 'pending' || state === 'approved'
}

async function newSession(): Promise<void> {
  if (running.value || scopeBusy.value) return
  if (binding.value || pending.value || scopeMode.value !== 'headless') await applyScope(true)
  else resetConversation()
}

function toolTone(status: string): 'ok' | 'warn' | 'danger' | 'neutral' {
  if (status === 'completed') return 'ok'
  if (status === 'failed') return 'danger'
  if (status === 'cancelled' || status === 'awaiting_confirmation' || status === 'unknown') return 'warn'
  return 'neutral'
}

onMounted(() => { clock = window.setInterval(() => { now.value = Date.now() }, 1000) })
onBeforeUnmount(() => {
  unmounted = true; controller.value?.abort(); scopeController.abort()
  if (clock !== null) window.clearInterval(clock)
})
</script>

<template>
  <Panel
    icon="chat" :title="t('chatTitle')" :hint="t('chatHint')">
    <template #actions>
      <Pill v-if="sessionId" size="sm" mono>{{ t('chatSession') }}: {{ sessionId.slice(0, 8) }}</Pill>
      <button type="button" class="btn" :disabled="running || scopeBusy" @click="newSession">{{ t('chatNew') }}</button>
    </template>

    <fieldset class="chat__scope" :disabled="running || scopeBusy">
      <legend>{{ t('chatScopeTitle') }}</legend>
      <p class="chat__scope-note">{{ t('chatScopeHint') }}</p>
      <Select v-model="scopeMode" :label="t('chatScopeChoose')" :options="scopeOptions" />
      <TextInput v-if="needsPerson" v-model="person" :label="t('chatScopePersonId')" :maxlength="120" />
      <TextInput v-if="needsDevice" v-model="device" :label="t('chatScopeDeviceId')" />
      <Toggle v-if="scopeMode === 'household'" v-model="householdWrite" :label="t('chatScopeGrant')" />
      <div class="chat__scope-actions">
        <button type="button" class="btn" :disabled="scopeBusy || running || (scopeMode !== 'headless' && !authToken)" @click="applyScope()">
          {{ scopeBusy ? t('loading') : pending ? t('chatScopeRetry') : t('chatScopeApply') }}
        </button>
        <button v-if="binding || pending" type="button" class="btn" :disabled="scopeBusy || running" @click="clearScope">{{ t('chatScopeRevoke') }}</button>
      </div>
    </fieldset>
    <p class="chat__scope-note" role="status">{{ t('chatScopeCurrent') }}: {{ scopeSummary }}
      <template v-if="binding"> · {{ binding.household_write ? t('chatScopeWriteGranted')
        : binding.scope.person_id || binding.scope.device_id ? t('chatScopeWriteRestricted') : t('chatScopeReadOnly') }}
        · {{ t('chatScopeExpires') }}: {{ new Date(binding.expires_ms).toLocaleString() }}</template>
    </p>
    <p v-if="!authToken && scopeMode !== 'headless'" class="chat__error">{{ t('chatScopePairing') }}</p>
    <p v-if="scopeDirty || pending" class="chat__scope-note">{{ t('chatScopeDraft') }}</p>
    <p v-if="expired || scopeInvalid" class="chat__error" role="alert">{{ t('chatScopeUnavailable') }}</p>
    <p v-if="scopeError" class="chat__error" role="alert">{{ scopeError }}</p>

    <div ref="listEl" class="chat__list">
      <EmptyState v-if="!bubbles.length" :title="t('chatEmpty')" />
      <div v-for="(bubble, index) in bubbles" :key="index" class="chat__bubble" :class="`chat__bubble--${bubble.role}`">
        <p class="chat__who">{{ bubble.role === 'user' ? t('chatYou') : t('chatAssistant') }}</p>
        <p class="chat__text">{{ bubble.text }}<span v-if="bubble.cancelled" class="chat__stopped"> {{ t('chatCancelled') }}</span></p>
        <div v-for="(tool, toolIndex) in bubble.tools" :key="toolIndex" class="chat__tool">
          <Pill :tone="toolTone(tool.status)" size="sm" mono>{{ t('chatTool') }}: {{ tool.tool }}</Pill>
          <span class="chat__tool-summary">{{ tool.summary }}</span>
        </div>
        <div v-if="bubble.confirmationId" class="chat__confirmation">
          <p>{{ confirmationSummary(bubble) }}</p>
          <RouterLink class="btn" :to="{ path: '/confirmations', query: { action: bubble.confirmationId } }">{{ t('chatReviewConfirmation') }}</RouterLink>
          <button v-if="pendingAction(bubble)" type="button" class="btn" @click="cancelPending(bubble)">{{ t('chatCancelPending') }}</button>
        </div>
        <button
          v-if="bubble.traceId"
          type="button"
          class="chat__trace mono"
          @click="emit('open-trace', bubble.traceId!)"
        >
          <AppIcon name="external" :size="12" /> {{ bubble.traceId }}
        </button>
      </div>
    </div>

    <div class="chat__composer">
      <label class="sr-only" for="chat-input">{{ t('chatPlaceholder') }}</label>
      <input
        id="chat-input"
        v-model="input"
        class="chat__input"
        :placeholder="t('chatPlaceholder')"
        :disabled="running"
        @keydown.enter.prevent="send"
      />
      <button v-if="!running" type="button" class="btn btn--primary" :disabled="!canSend" @click="send">
        {{ t('chatSend') }}
      </button>
      <button v-else type="button" class="btn btn--danger" @click="stop">{{ t('chatStop') }}</button>
    </div>
    <p v-if="sendError" class="chat__error" role="alert">{{ sendError }}</p>
  </Panel>
</template>

<style scoped>
.chat__list {
  max-height: 26rem;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.chat__bubble {
  max-width: 82%;
  padding: var(--sp-2) var(--sp-3);
  border-radius: var(--r-md);
  background: var(--surface);
  border: 1px solid var(--line);
  white-space: pre-wrap;
}

.chat__bubble--user {
  margin-left: auto;
  background: var(--accent-soft);
  border-color: var(--accent-line);
}

.chat__who {
  font-size: var(--fs-xs);
  color: var(--muted);
  margin-bottom: var(--sp-1);
}

.chat__text {
  font-size: var(--fs-base);
  word-break: break-word;
}

.chat__stopped {
  color: var(--warn);
}

.chat__tool {
  display: flex;
  align-items: baseline;
  gap: var(--sp-2);
  flex-wrap: wrap;
  margin-top: var(--sp-2);
}

.chat__tool-summary {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.chat__trace {
  margin-top: var(--sp-2);
  border: 0;
  background: none;
  color: var(--accent);
  font-size: var(--fs-xs);
  padding: 0;
  cursor: pointer;
}

.chat__confirmation { display: flex; flex-wrap: wrap; gap: var(--sp-2); margin-top: var(--sp-3); }
.chat__confirmation p { flex-basis: 100%; }

.chat__composer {
  display: flex;
  gap: var(--sp-2);
}

.chat__input {
  min-width: 0;
  flex: 1;
  background: var(--surface-inset);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--ink);
  padding: 0.35rem var(--sp-3);
}

.chat__scope { display: grid; gap: var(--sp-2); min-width: 0; border: 1px solid var(--line); border-radius: var(--r-sm); padding: var(--sp-3); }
.chat__scope-actions { display: flex; flex-wrap: wrap; gap: var(--sp-2); }
.chat__scope-note { color: var(--muted); font-size: var(--fs-sm); overflow-wrap: anywhere; }
.chat__error { color: var(--danger); font-size: var(--fs-sm); overflow-wrap: anywhere; }
@media (max-width: 400px) {
  .chat__bubble { max-width: 100%; }
  .chat__scope-actions .btn { width: 100%; }
  .chat__confirmation .btn { min-height: 44px; }
  .chat__composer { flex-wrap: wrap; }
  .chat__input { flex-basis: 100%; }
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.24rem var(--sp-4);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn--primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.btn--danger {
  background: var(--danger);
  border-color: var(--danger);
  color: var(--surface);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
