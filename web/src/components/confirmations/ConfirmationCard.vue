<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { ApiError } from '../../api'
import { authToken } from '../../auth'
import { t, locale, type Key } from '../../i18n'
import { errorDetail } from '../../format'
import { usePoll } from '../../hooks/usePoll'
import { changeConfirmation, confirmationActions, confirmationBusy, confirmationErrors,
  confirmationUnavailable, refreshConfirmation } from '../../stores/confirmations'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'

const props = defineProps<{ id: string }>()
const action = computed(() => confirmationActions.get(props.id))
const error = computed(() => confirmationErrors.get(props.id))
const busy = computed(() => confirmationBusy.has(props.id))
const now = ref(Date.now())
let clock: number | null = null
onMounted(() => { clock = window.setInterval(() => { now.value = Date.now() }, 1000) })
onBeforeUnmount(() => { if (clock !== null) window.clearInterval(clock) })
usePoll(() => {
  const current = action.value
  if (!current || error.value || ['pending', 'approved'].includes(current.state)
      || (current.state === 'consumed' && (!current.outcome || current.outcome.status === 'running'))) {
    return refreshConfirmation(props.id)
  }
}, 3000)
const elapsed = computed(() => Boolean(action.value && now.value >= action.value.expires_ms))
const actionable = computed(() => Boolean(authToken.value && action.value && !error.value && !busy.value && !elapsed.value))
const labels: Record<string, Key> = {
  pending: 'confirmationPending', approved: 'confirmationApproved', consumed: 'confirmationConsumed',
  rejected: 'confirmationRejected', expired: 'confirmationExpired', cancelled: 'confirmationCancelled', stale: 'confirmationStale',
}
const evidence: Record<string, Key> = { accepted: 'confirmationAccepted', sent: 'confirmationSent',
  observed: 'confirmationObserved', device_confirmed: 'confirmationDeviceConfirmed',
  submitted: 'confirmationSubmitted', unknown: 'confirmationEvidenceUnknown' }
const outcome = computed(() => action.value?.outcome)
const outcomeLabel = computed<Key | null>(() => {
  const result = outcome.value
  if (!result) return null
  switch (result.status) {
    case 'unknown': return 'confirmationUnknown'
    case 'partial': return 'confirmationPartial'
    case 'failed': return 'confirmationFailed'
    case 'cancelled': return 'confirmationOutcomeCancelled'
    case 'completed': return result.fact?.confirmed && result.evidence === 'device_confirmed'
      ? 'confirmationConfirmed' : 'confirmationUnconfirmed'
    default: return null
  }
})
const statusLabel = computed<Key>(() => {
  const current = action.value
  if (current?.state === 'consumed' && outcome.value?.status === 'unknown') return 'confirmationStatusUnknown'
  if (current?.state === 'consumed' && outcome.value && outcome.value.status !== 'running') return 'confirmationRecorded'
  return labels[current?.state ?? 'pending']!
})
const message = computed(() => {
  if (error.value instanceof ApiError && error.value.status === 401) return t('confirmationsPairing')
  if (confirmationUnavailable(error.value)) return t('confirmationsUnavailable')
  return error.value ? `${t('confirmationsStale')} ${errorDetail(error.value)}` : ''
})
const expiry = computed(() => action.value ? new Date(action.value.expires_ms).toLocaleString(locale.value === 'zh' ? 'zh-CN' : 'en') : '')
const scope = computed(() => {
  const s = action.value?.preview.scope
  return s ? [s.person_id ? `${t('chatScopePersonId')}: ${s.person_id}` : '',
    s.device_id ? `${t('chatScopeDeviceId')}: ${s.device_id}` : '', s.room ?? ''].filter(Boolean).join(' · ') : ''
})
</script>

<template>
  <Panel :id="`confirmation-${id}`" :title="action?.preview.operation ?? t('confirmationsTitle')" icon="tools"
    :tone="error || outcome?.status === 'unknown' ? 'warn' : 'default'" :aria-busy="busy">
    <p v-if="message" role="alert">{{ message }}</p>
    <template v-if="action">
      <div class="confirmation__status" role="status">
        <Pill :tone="action.state === 'pending' || action.state === 'approved' || outcome?.status === 'unknown' ? 'warn' : 'neutral'">
          {{ t(statusLabel) }}
        </Pill>
        <span v-if="busy">{{ t('confirmationBusy') }}</span>
      </div>
      <dl class="confirmation__facts">
        <dt>{{ t('confirmationOriginal') }}</dt>
        <dd>{{ action.preview.service }} · {{ action.preview.method }} {{ action.preview.target }}</dd>
        <dt>{{ t('confirmationRequester') }}</dt>
        <dd>{{ action.requester.kind === 'physical' ? t('confirmationPhysical') : t('confirmationManagement') }}
          <template v-if="action.requester.device_id"> · {{ action.requester.device_id }}</template></dd>
        <dt>{{ t('confirmationScope') }}</dt><dd>{{ scope || t('confirmationNoScope') }}</dd>
        <dt>{{ t('confirmationExpires') }}</dt><dd>{{ expiry }}</dd>
        <dt>{{ t('confirmationTask') }}</dt><dd class="mono">{{ action.task_id }}</dd>
      </dl>
      <details open>
        <summary>{{ t('confirmationParameters') }}</summary>
        <pre class="confirmation__parameters">{{ JSON.stringify(action.preview.parameters, null, 2) }}</pre>
      </details>
      <p v-if="action.state === 'approved'">{{ t('confirmationApprovalHint') }}</p>
      <p v-if="elapsed && ['pending', 'approved'].includes(action.state)" role="status">{{ t('confirmationLocalExpiry') }}</p>
      <div v-if="action.state === 'consumed'" class="confirmation__outcome" role="status">
        <p>{{ t('confirmationNoRecall') }}</p>
        <p v-if="outcomeLabel">{{ t(outcomeLabel) }}</p>
        <template v-if="outcome">
          <p>{{ t('confirmationEvidence') }}: {{ evidence[outcome.evidence] ? t(evidence[outcome.evidence]!) : outcome.evidence }}</p>
          <p v-if="outcome.fact">{{ outcome.fact.subject }}<template v-if="outcome.fact.value"> · {{ outcome.fact.value }}</template>
            <template v-if="outcome.fact.detail"> · {{ outcome.fact.detail }}</template></p>
          <p v-if="outcome.error">{{ outcome.error.code }}: {{ outcome.error.detail }}</p>
        </template>
      </div>
      <div class="confirmation__buttons">
        <button v-if="action.state === 'pending'" type="button" class="btn btn--primary" :disabled="!actionable"
          @click="changeConfirmation(id, 'approve')">{{ t('confirmationApprove') }}</button>
        <button v-if="action.state === 'approved'" type="button" class="btn btn--danger" :disabled="!actionable"
          @click="changeConfirmation(id, 'execute')">{{ t('confirmationExecute') }}</button>
        <button v-if="action.state === 'pending' || action.state === 'approved'" type="button" class="btn" :disabled="!actionable"
          @click="changeConfirmation(id, 'reject')">{{ t('confirmationReject') }}</button>
        <button v-if="action.state === 'pending' || action.state === 'approved'" type="button" class="btn" :disabled="!actionable"
          @click="changeConfirmation(id, 'cancel')">{{ t('confirmationCancel') }}</button>
      </div>
    </template>
    <button type="button" class="btn" :disabled="!authToken || busy" @click="refreshConfirmation(id)">{{ t('confirmationRefresh') }}</button>
  </Panel>
</template>

<style scoped>
.confirmation__status, .confirmation__buttons { display: flex; gap: var(--sp-2); flex-wrap: wrap; align-items: center; }
.confirmation__facts { display: grid; grid-template-columns: minmax(7rem, auto) minmax(0, 1fr); gap: var(--sp-2) var(--sp-3); }
.confirmation__facts dt { color: var(--muted); }
.confirmation__facts dd { margin: 0; overflow-wrap: anywhere; }
.confirmation__parameters { white-space: pre-wrap; overflow-wrap: anywhere; max-height: 18rem; overflow: auto; padding: var(--sp-3); background: var(--surface-2); }
.confirmation__outcome { display: grid; gap: var(--sp-2); }
summary { cursor: pointer; padding-block: var(--sp-2); }
@media (max-width: 540px) {
  .confirmation__facts { grid-template-columns: 1fr; gap: var(--sp-1); }
  .confirmation__facts dd { margin-bottom: var(--sp-2); }
  .btn { min-height: 44px; }
  .confirmation__buttons .btn { flex: 1 1 100%; min-height: 44px; }
}
</style>
