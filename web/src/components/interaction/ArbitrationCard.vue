<script setup lang="ts">
/**
 * Multi-speaker wake coordination (arbitration).
 *
 * Shows the coordination mode (off / observe / enforce), distinguishes
 * configured fleet peers from currently reachable peers, and displays recent
 * `arbitration_*` decision events from `/api/v0/voice/events`.
 */
import { computed, ref, watch } from 'vue'
import {
  getFleet,
  getVoiceEvents,
  putMode,
  type ModeDevice,
  type ModeSnapshot,
  type VoiceEvent,
} from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

type Mode = 'off' | 'observe' | 'enforce'

const choices = [
  { value: 'off', label: 'arbOff', note: 'arbOffNote' },
  { value: 'observe', label: 'arbObserve', note: 'arbObserveNote' },
  { value: 'enforce', label: 'arbEnforce', note: 'arbEnforceNote' },
] as const

const value = ref<Mode | null>(null)
const touched = ref(false)
const { busy, message, tone, run } = useSave()

const fleet = useResource((signal) => getFleet({ signal }), { timeoutMs: 5000, retries: 1 })
const events = useResource(
  (signal) => getVoiceEvents(Date.now() - 24 * 3600_000, 200, { signal, tail: true }),
  { timeoutMs: 6000, retries: 1 },
)
usePoll(() => fleet.load(), 15000)
usePoll(() => events.load(), 10000)

const registeredPeers = computed(() => props.mode?.wake_arbitration?.peers ?? 0)
const configuredPeers = computed(() => fleet.data.value?.nodes?.length ?? registeredPeers.value)
const reachablePeers = computed(() => {
  const nodes = fleet.data.value?.nodes
  if (!nodes) return registeredPeers.value
  return nodes.filter((n) => n.last_ok_ms > 0 && !n.last_error).length
})

interface ArbDecisionRow {
  key: string
  tsMs: number
  kind: string
  winner: string
  winnerRoom: string
  reason: string
  margin: string
}

function asRecord(v: unknown): Record<string, unknown> | null {
  return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null
}

function str(v: unknown): string {
  return typeof v === 'string' ? v : ''
}

function num(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}

const recentArbitrations = computed<ArbDecisionRow[]>(() => {
  const list = events.data.value?.events ?? []
  const filtered = list.filter(
    (ev: VoiceEvent) => ev.kind.startsWith('arbitration_') || ev.kind === 'sovereignty_decision',
  )
  return filtered
    .slice(-10)
    .reverse()
    .map((ev, idx) => {
      const dec = asRecord(ev.decision)
      const winner = str(dec?.winner) || str(ev.room_owner) || str(ev.device) || '—'
      const winnerRoom =
        str(dec?.winner_room) || str(ev.room_owner_room) || str(ev.room_label_display)
      const reason = str(dec?.why) || str(ev.reason) || str(ev.why) || '—'
      const marginMs = num(dec?.margin_ms)
      const marginDb = num(dec?.margin_db)
      const parts: string[] = []
      if (marginMs !== null) parts.push(`${Math.round(marginMs)} ms`)
      if (marginDb !== null) parts.push(`${marginDb.toFixed(1)} dB`)
      return {
        key: `${ev.ts_ms}-${ev.seq ?? idx}-${ev.kind}`,
        tsMs: ev.ts_ms,
        kind: ev.kind,
        winner,
        winnerRoom,
        reason,
        margin: parts.join(' · ') || '—',
      }
    })
})

function kindLabel(kind: string): string {
  switch (kind) {
    case 'arbitration_won':
      return t('arbEventWon')
    case 'arbitration_lost':
      return t('arbEventLost')
    case 'arbitration_tiebreak':
      return t('arbEventTiebreak')
    case 'arbitration_observed':
      return t('arbEventObserved')
    case 'arbitration_unavailable':
      return t('arbEventUnavailable')
    case 'sovereignty_decision':
      return t('arbEventSovereignty')
    default:
      return kind
  }
}

function reasonLabel(reason: string): string {
  switch (reason) {
    case 'louder':
      return t('arbReasonLouder')
    case 'louder_uncalibrated':
      return t('arbReasonLouderUncalibrated')
    case 'first':
      return t('arbReasonFirst')
    case 'tie_break':
      return t('arbReasonTieBreak')
    case 'session_sticky':
      return t('arbReasonSessionSticky')
    case 'solo':
      return t('arbReasonSolo')
    case 'abstained_bleed':
      return t('arbReasonAbstainedBleed')
    case 'owner_room_sticky':
      return t('arbReasonOwnerRoomSticky')
    case 'acoustic_winner':
      return t('arbReasonAcousticWinner')
    case 'incomplete_fallback':
      return t('arbReasonIncompleteFallback')
    default:
      return reason
  }
}

function kindTone(kind: string): 'ok' | 'warn' | 'info' | 'muted' {
  switch (kind) {
    case 'arbitration_won':
    case 'arbitration_tiebreak':
      return 'ok'
    case 'arbitration_observed':
    case 'sovereignty_decision':
      return 'info'
    case 'arbitration_unavailable':
      return 'warn'
    default:
      return 'muted'
  }
}

watch(
  () => props.mode,
  (mode) => {
    if (!mode) return
    if (!touched.value) value.value = mode.wake_arbitration?.mode ?? 'off'
  },
  { immediate: true },
)

function pick(v: Mode): void {
  touched.value = true
  value.value = v
}

async function save(): Promise<void> {
  if (!props.device || value.value === null) return
  await run(async () => {
    await putMode(props.device!.device, { wake_arbitration: value.value! })
    touched.value = false
    await props.refresh()
  })
}
</script>

<template>
  <Panel
    icon="wake"
    apply="immediate"
    :title="t('arbTitle')"
    :hint="t('arbHint')"
  >
    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <div class="arb__options">
        <button
          v-for="c in choices"
          :key="c.value"
          type="button"
          class="arb__choice"
          :class="{ 'arb__choice--on': value === c.value }"
          :disabled="busy || value === null"
          @click="pick(c.value)"
        >
          <span class="arb__radio" />
          <span class="arb__label">{{ t(c.label) }}</span>
          <span class="arb__note">{{ t(c.note) }}</span>
        </button>
      </div>

      <p
        class="arb__fact"
        :class="reachablePeers > 0 ? 'arb__fact--ok' : 'arb__fact--muted'"
      >
        {{
          configuredPeers > 0
            ? t('arbPeers', { configured: configuredPeers, reachable: reachablePeers })
            : t('arbNoPeers')
        }}
      </p>

      <p class="arb__global">{{ t('arbGlobal') }}</p>

      <div class="arb__actions">
        <button type="button" class="btn btn--primary" :disabled="busy || value === null" @click="save">
          {{ busy ? t('saving') : t('arbSave') }}
        </button>
        <span v-if="message" class="arb__msg" :class="`arb__msg--${tone}`">{{ message }}</span>
      </div>

      <div class="arb__events">
        <div class="arb__events-head">
          <span class="arb__events-title">{{ t('arbEventsTitle') }}</span>
          <button type="button" class="btn btn--ghost btn--compact" @click="events.load()">
            {{ t('refresh') }}
          </button>
        </div>
        <p v-if="!recentArbitrations.length" class="arb__events-empty">{{ t('arbEventsEmpty') }}</p>
        <ul v-else class="arb__event-list">
          <li v-for="row in recentArbitrations" :key="row.key" class="arb__event">
            <span class="mono arb__event-time">{{ formatTime(row.tsMs) }}</span>
            <Pill size="sm" :tone="kindTone(row.kind)">{{ kindLabel(row.kind) }}</Pill>
            <span class="arb__event-winner">
              {{ row.winner }}<template v-if="row.winnerRoom"> ({{ row.winnerRoom }})</template>
            </span>
            <span class="arb__event-reason">{{ reasonLabel(row.reason) }}</span>
            <span v-if="row.margin !== '—'" class="mono arb__event-margin">{{ row.margin }}</span>
          </li>
        </ul>
        <p v-if="recentArbitrations.length" class="arb__events-note">{{ t('arbRoomLabelNote') }}</p>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.arb__options {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.arb__choice {
  display: grid;
  grid-template-columns: auto 1fr;
  grid-template-rows: auto auto;
  column-gap: var(--sp-2);
  row-gap: 0.15rem;
  align-items: baseline;
  text-align: left;
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  color: var(--ink);
  cursor: pointer;
}

.arb__choice--on {
  border-color: var(--accent);
}

.arb__radio {
  grid-row: span 2;
  width: 0.75rem;
  height: 0.75rem;
  border-radius: 50%;
  border: 1px solid var(--line-strong);
  align-self: center;
}

.arb__choice--on .arb__radio {
  border-color: var(--accent);
  background: var(--accent);
}

.arb__label {
  font-size: var(--fs-sm);
  font-weight: 600;
}

.arb__note {
  font-size: var(--fs-sm);
  color: var(--muted);
  line-height: 1.45;
}

.arb__fact {
  font-size: var(--fs-sm);
  border-left: 3px solid var(--line-strong);
  padding-left: var(--sp-2);
}

.arb__fact--ok {
  border-left-color: var(--ok);
}

.arb__fact--muted {
  border-left-color: var(--warn);
}

.arb__global {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.arb__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.arb__choice:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.arb__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.arb__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.arb__events {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding-top: var(--sp-2);
  border-top: 1px solid var(--line);
}

.arb__events-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-2);
}

.arb__events-title {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--ink-2);
}

.arb__events-empty {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.arb__events-note {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.arb__event-list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.arb__event {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-1) var(--sp-2);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  font-size: var(--fs-xs);
}

.arb__event-time {
  color: var(--muted);
}

.arb__event-winner {
  font-weight: 600;
  color: var(--ink);
}

.arb__event-reason {
  color: var(--ink-2);
}

.arb__event-margin {
  color: var(--muted);
  margin-left: auto;
}
</style>
