<script setup lang="ts">
/**
 * Request traces: the list, then one trace's events. A row opens the detail;
 * the detail is an ordered evidence log, not a summary.
 */
import { ref, watch } from 'vue'
import { getTrace, getTraces, type TraceDetail } from '../../api'
import { t } from '../../i18n'
import { errorDetail, formatDateTime, formatTime } from '../../format'
import { useResource } from '../../hooks/useResource'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = withDefaults(defineProps<{ focus?: string | null }>(), { focus: null })

const list = useResource((signal) => getTraces({ signal }), { timeoutMs: 6000, retries: 1 })
void list.load()

const detail = ref<TraceDetail | null>(null)
const detailBusy = ref(false)
const detailError = ref('')

watch(
  () => props.focus,
  (id) => {
    if (id) void open(id)
  },
)

async function open(id: string): Promise<void> {
  detailBusy.value = true
  detailError.value = ''
  try {
    detail.value = await getTrace(id)
  } catch (err) {
    detailError.value = errorDetail(err)
  } finally {
    detailBusy.value = false
  }
}

function statusTone(status: string): 'ok' | 'warn' | 'danger' | 'accent' | 'neutral' {
  switch (status) {
    case 'completed':
      return 'ok'
    case 'cancelled':
      return 'warn'
    case 'failed':
      return 'danger'
    case 'running':
      return 'accent'
    default:
      return 'neutral'
  }
}

function wakeLabel(trace: { wake_channel?: string | null; wake_word?: string | null }): string {
  const ch = trace.wake_channel ?? ''
  const word = trace.wake_word ?? ''
  if (!ch && !word) return ''
  if (ch === 'followup') return t('traceWakeFollowup')
  const tag = ch === 'kws' ? t('traceWakeKws') : ch === 'asr' ? t('traceWakeAsr') : ch
  return word ? `${tag} · ${word}` : tag
}

function eventSummary(kind: string, payload: Record<string, unknown>): string {
  const text = (value: unknown): string => (typeof value === 'string' ? value : '')
  switch (kind) {
    case 'turn_start': {
      const wake = wakeLabel({
        wake_channel: text(payload.wake_channel) || null,
        wake_word: text(payload.wake_word) || null,
      })
      return wake ? `${text(payload.source)} · ${wake}` : text(payload.source)
    }
    case 'tool_start':
      return `${text(payload.tool)} ${JSON.stringify(payload.args ?? {})}`
    case 'tool_result':
      return `${text(payload.tool)} · ${text(payload.status)} / ${text(payload.evidence)}${
        payload.late ? ' · late' : ''
      }\n${text(payload.summary)}`
    case 'llm_end':
      return `finish=${text(payload.finish)} tools=${String(payload.tool_calls ?? '')}`
    case 'llm_start':
      return `context=${String(payload.context_messages ?? '')} msgs`
    default:
      return JSON.stringify(payload)
  }
}
</script>

<template>
  <Panel
    icon="traces" :title="t('tracesTitle')" :hint="t('tracesHint')">
    <template #actions>
      <button type="button" class="btn" :disabled="list.loading.value" @click="list.load()">
        <AppIcon name="reload" :size="13" /> {{ t('refresh') }}
      </button>
    </template>

    <template v-if="!detail">
      <LoadingBlock v-if="list.loading.value && !list.data.value" :label="t('loading')" />
      <ErrorNotice
        v-else-if="list.error.value && !list.data.value"
        :error="list.error.value"
        @retry="list.load()"
      />
      <EmptyState v-else-if="!list.data.value?.traces.length" :title="t('noTraces')" />
      <div v-else class="traces__table">
        <div class="traces__head">
          <span>{{ t('traceStatus') }}</span>
          <span>{{ t('traceInput') }}</span>
          <span>{{ t('traceSource') }}</span>
          <span>{{ t('traceDuration') }}</span>
        </div>
        <button
          v-for="trace in list.data.value?.traces ?? []"
          :key="trace.id"
          type="button"
          class="traces__row"
          @click="open(trace.id)"
        >
          <Pill :tone="statusTone(trace.status)" size="sm" mono>{{ trace.status }}</Pill>
          <span class="traces__input">
            <Pill v-if="wakeLabel(trace)" tone="accent" size="sm">{{ wakeLabel(trace) }}</Pill>
            {{ trace.input_text }}
          </span>
          <span class="mono traces__source">{{ trace.source }}</span>
          <span class="mono">{{ formatDateTime(trace.created_ms) }} · {{ trace.duration_ms }} ms</span>
        </button>
      </div>
    </template>

    <template v-else>
      <div class="traces__detail-head">
        <button type="button" class="btn" @click="detail = null">
          <AppIcon name="back" :size="14" /> {{ t('traceBack') }}
        </button>
        <Pill :tone="statusTone(detail.status)" size="sm" mono>{{ detail.status }}</Pill>
        <span class="mono traces__id">{{ detail.id }}</span>
      </div>
      <p class="traces__meta">
        {{ t('traceInput') }}: <b>{{ detail.input_text }}</b>
      </p>
      <p class="traces__meta mono">
        {{ t('traceSource') }}: {{ detail.source }}
        <template v-if="wakeLabel(detail)"> · {{ t('traceWake') }}: {{ wakeLabel(detail) }}</template>
        · {{ t('outputTask') }}: {{ detail.task_id }} ·
        {{ t('traceDuration') }}: {{ detail.duration_ms }} ms
      </p>

      <p class="traces__subtitle">{{ t('traceEvents') }}</p>
      <EmptyState v-if="!detail.events.length" :title="t('traceNoEvents')" />
      <ol v-else class="traces__events">
        <li v-for="event in detail.events" :key="event.seq" class="traces__event">
          <span class="mono traces__time">{{ formatTime(event.ts_ms) }}</span>
          <span class="traces__kind">{{ event.kind }}</span>
          <span class="mono traces__payload">{{ eventSummary(event.kind, event.payload) }}</span>
        </li>
      </ol>
    </template>

    <p v-if="detailBusy" class="traces__meta">{{ t('loading') }}</p>
    <p v-if="detailError" class="traces__error">{{ detailError }}</p>
  </Panel>
</template>

<style scoped>
.traces__table {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  overflow-x: auto;
}

.traces__head,
.traces__row {
  display: grid;
  grid-template-columns: 7rem minmax(14rem, 2fr) 7rem 16rem;
  gap: var(--sp-2);
  align-items: center;
  min-width: 46rem;
}

.traces__head {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
  border-bottom: 1px solid var(--line);
  padding-bottom: var(--sp-1);
}

.traces__row {
  border: 0;
  border-bottom: 1px solid var(--line);
  background: none;
  color: inherit;
  text-align: left;
  padding: var(--sp-2) 0;
  cursor: pointer;
  font-size: var(--fs-sm);
}

.traces__row:hover {
  background: var(--surface-2);
}

.traces__input {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.traces__source {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.traces__detail-head {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.traces__id {
  color: var(--muted);
  font-size: var(--fs-sm);
}

.traces__meta {
  font-size: var(--fs-sm);
  color: var(--ink-2);
}

.traces__subtitle {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
}

.traces__events {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.traces__event {
  display: grid;
  grid-template-columns: 5.5rem 9rem minmax(12rem, 1fr);
  gap: var(--sp-2);
  padding: var(--sp-2);
  border-left: 3px solid var(--line-strong);
  border-radius: 0 var(--r-sm) var(--r-sm) 0;
  background: var(--surface-2);
  font-size: var(--fs-sm);
}

@media (max-width: 640px) {
  .traces__event {
    grid-template-columns: 1fr;
  }
}

.traces__time {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.traces__kind {
  color: var(--accent);
  font-size: var(--fs-xs);
  font-weight: 600;
}

.traces__payload {
  font-size: var(--fs-xs);
  white-space: pre-wrap;
  word-break: break-word;
}

.traces__error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.2rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
