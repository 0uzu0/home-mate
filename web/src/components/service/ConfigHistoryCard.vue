<script setup lang="ts">
/**
 * Change history: every save, probe and restore. Restore replays config
 * values only; it is confirmed through the dialog, and a 409/429 is surfaced
 * as the server sent it.
 */
import { ref } from 'vue'
import {
  getConfigHistory,
  restoreConfigEntry,
  type ConfigHistoryEntry,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail, formatDateTime } from '../../format'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import { useSave } from '../../hooks/useSave'
import { useCooldown } from '../../hooks/useCooldown'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import EmptyState from '../EmptyState.vue'
import Pill from '../Pill.vue'
import ProvenanceTag from '../ProvenanceTag.vue'
import ConfirmDialog from '../ConfirmDialog.vue'

const history = useResource((signal) => getConfigHistory({ signal }), {
  timeoutMs: 6000,
  retries: 1,
})
usePoll(() => history.load(), 15000)

const pending = ref<ConfigHistoryEntry | null>(null)
const { busy, message, tone, run } = useSave()
const cooldown = useCooldown()

/** The restore route supports these scopes only; every other row used to
 * offer a button that always ended in a conflict (audit 2026-11-14). */
const RESTORABLE = ['llm', 'llm_fallback', 'asr', 'tts', 'device', 'voice']
function restorable(scope: string): boolean {
  return RESTORABLE.includes(scope)
}

function askRestore(entry: ConfigHistoryEntry): void {
  if (!entry.payload || Object.keys(entry.payload).length === 0) {
    message.value = t('historyNoPayload')
    tone.value = 'error'
    return
  }
  pending.value = entry
}

async function confirmRestore(): Promise<void> {
  const entry = pending.value
  if (!entry) return
  await run(async () => {
    try {
      const result = await restoreConfigEntry(entry.id)
      // A restore can change any card on this page; reload so every draft is
      // re-read from the server instead of showing pre-restore values.
      window.setTimeout(() => window.location.reload(), 1200)
      return t('historyRestored', { id: entry.id, scope: result.scope })
    } catch (err) {
      cooldown.fromError(err)
      throw err
    } finally {
      pending.value = null
      await history.load()
    }
  })
}
</script>

<template>
  <Panel
    icon="history" :title="t('historyTitle')" :hint="t('historyHint')">
    <template #actions>
      <button type="button" class="btn" :disabled="history.loading.value" @click="history.load()">
        <AppIcon name="reload" :size="13" /> {{ t('refresh') }}
      </button>
    </template>

    <div v-if="history.error.value && !history.data.value">
      <p class="history__error">{{ errorDetail(history.error.value) }}</p>
    </div>

    <EmptyState v-else-if="!history.data.value?.history.length" :title="t('noHistory')" />

    <div v-else class="history__table">
      <div class="history__head">
        <span>{{ t('historyTime') }}</span>
        <span>{{ t('historyActor') }}</span>
        <span>{{ t('historyScope') }}</span>
        <span>{{ t('historySummary') }}</span>
        <span>{{ t('actions') }}</span>
      </div>
      <div v-for="entry in history.data.value?.history ?? []" :key="entry.id" class="history__row">
        <span class="mono history__time">{{ formatDateTime(entry.ts_ms) }}</span>
        <ProvenanceTag :source="entry.actor" />
        <Pill size="sm" mono>{{ entry.scope }}</Pill>
        <span class="history__summary">{{ entry.summary }}</span>
        <button
          type="button"
          class="btn"
          :disabled="busy || cooldown.active.value || !restorable(entry.scope)"
          :title="restorable(entry.scope) ? '' : t('historyRestoreUnsupported')"
          @click="askRestore(entry)"
        >
          <AppIcon name="history" :size="13" /> {{ cooldown.active.value ? `${cooldown.seconds.value}s` : t('historyRestore') }}
        </button>
      </div>
    </div>

    <p v-if="message" class="history__msg" :class="`history__msg--${tone}`">{{ message }}</p>

    <ConfirmDialog
      v-if="pending"
      :title="t('historyRestoreTitle')"
      :message="t('historyRestoreBody')"
      :confirm-label="t('historyRestore')"
      :cancel-label="t('cancel')"
      :busy="busy"
      :busy-label="t('saving')"
      @confirm="confirmRestore"
      @cancel="pending = null"
    />
  </Panel>
</template>

<style scoped>
.history__table {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  overflow-x: auto;
}

.history__head,
.history__row {
  display: grid;
  grid-template-columns: 10rem 7rem 7rem minmax(12rem, 1fr) 6rem;
  gap: var(--sp-2);
  align-items: center;
  min-width: 46rem;
}

.history__head {
  font-size: var(--fs-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--muted);
  border-bottom: 1px solid var(--line);
  padding-bottom: var(--sp-1);
}

.history__row {
  font-size: var(--fs-sm);
  padding: var(--sp-1) 0;
  border-bottom: 1px solid var(--line);
}

.history__time {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.history__summary {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
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

.history__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.history__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.history__error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
