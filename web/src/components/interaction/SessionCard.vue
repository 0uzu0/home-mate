<script setup lang="ts">
import { computed } from 'vue'
import { closeSession, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
const props = defineProps<{ device: ModeDevice; refresh: () => void | Promise<void> }>()
const { busy, message, tone, run } = useSave()
const session = computed(() => props.device.session)
const phase = computed(() => {
  switch (session.value?.phase) {
    case 'closed': return t('sessionIdle')
    case 'active': return t('sessionActive')
    case 'tail': return t('sessionTail')
    case 'closing': return t('sessionClosingPhase')
    default: return '—'
  }
})
const inputState = computed(() => {
  switch (props.device.input?.state) {
    case 'capture': return t('inputCapturing')
    case 'recognizing': return t('inputRecognizing')
    case 'cancelled': return t('inputCancelled')
    case 'expired': return t('inputExpired')
    default: return t('inputIdle')
  }
})
const opener = computed(() => {
  switch (session.value?.opened_by) {
    case 'keyword': return t('sessionByKeyword')
    case 'asr_wake': return t('sessionByAsr')
    case 'protocol_wake': return t('sessionByProtocol')
    case 'text_wake': return t('sessionByText')
    case 'explicit_activation': return t('sessionExplicit')
    default: return session.value?.opened_by ?? '—'
  }
})
const denial = computed(() => {
  switch (session.value?.last_denied?.reason) {
    case 'session_closed': return t('sessionReasonClosed')
    case 'media_lifecycle': return t('sessionReasonMedia')
    case 'followup_credits_exhausted': return t('sessionReasonCredits')
    default: return session.value?.last_denied?.reason ?? '—'
  }
})
async function close(): Promise<void> {
  await run(async () => {
    try { await closeSession(props.device.device, session.value?.generation) }
    finally { await props.refresh() }
    return t('sessionCloseHint')
  })
}
</script>
<template>
  <Panel icon="wake" :title="t('sessionTitle')" :hint="t('sessionHint')">
    <dl aria-live="polite">
      <dt>{{ t('sessionPhase') }}</dt><dd>{{ phase }}</dd>
      <dt>{{ t('inputWindow') }}</dt><dd>{{ device.input_window_open ? t('inputWindowOpen') : t('inputWindowClosed') }}</dd>
      <dt v-if="device.media_guard">{{ t('mediaGuard') }}</dt><dd v-if="device.media_guard">{{ device.media_guard.uncertain ? t('mediaUncertain') : device.media_guard.guard_remaining_ms > 0 ? t('mediaTailGuard') : t('mediaSettled') }}</dd>
      <dt>{{ t('inputState') }}</dt><dd>{{ inputState }}<span v-if="device.input?.remaining_ms != null"> · {{ Math.ceil(device.input.remaining_ms / 1000) }}s</span></dd>
      <dt>{{ t('sessionOpener') }}</dt><dd>{{ opener }}</dd>
      <dt>{{ t('sessionCredits') }}</dt><dd>{{ session?.credits ?? '—' }}</dd>
      <dt>{{ t('sessionDeadline') }}</dt><dd>{{ session?.tail_deadline_ms ? formatTime(session.tail_deadline_ms) : session?.phase === 'active' ? t('sessionRunning') : '—' }}</dd>
      <dt>{{ t('sessionDenied') }}</dt><dd>{{ denial }}<span v-if="session?.last_denied"> · {{ t('sessionWakeAgain') }}</span></dd>
    </dl>
    <button type="button" :disabled="busy || !session" @click="close">{{ busy ? t('saving') : t('sessionClose') }}</button>
    <p v-if="message" role="status" :class="tone">{{ message }}</p>
  </Panel>
</template>
<style scoped>
dl { display: grid; grid-template-columns: max-content 1fr; gap: var(--sp-2) var(--sp-4); }
dt { color: var(--muted); } dd { margin: 0; }
button { align-self: flex-start; border: 1px solid var(--accent); border-radius: var(--r-sm); background: var(--accent); color: var(--on-accent); padding: var(--sp-1) var(--sp-3); cursor: pointer; }
button:disabled { opacity: .5; cursor: not-allowed; }
.error { color: var(--danger); } .ok { color: var(--ok); }
</style>
