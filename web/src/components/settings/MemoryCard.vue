<script setup lang="ts">
/**
 * Household memory (2026-11-13, xiaozhi-style).
 *
 * Short, user-confirmed notes only: names and the correct way to say them,
 * preferences, habits, important events. It is NOT a log of what the assistant
 * did — service availability, tool receipts and playback state belong to
 * config/state and change on their own; remembering them is how a speaker
 * starts insisting that something "is not connected" after it was configured.
 *
 * Management lists all scopes; each prompt receives only the subset allowed
 * by its trusted execution context (bounded, pinned first).
 */
import { computed, ref } from 'vue'
import {
  addMemory,
  clearMemories,
  confirmMemoryCandidate,
  deleteMemory,
  getMemories,
  getMemoryCandidates,
  rejectMemoryCandidate,
  updateMemory,
  type MemoryCandidate,
  type MemoryItem,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Select from '../Select.vue'
import TextInput from '../TextInput.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'
import ConfirmDialog from '../ConfirmDialog.vue'

const list = useResource((signal) => getMemories({ signal }), { timeoutMs: 6000, retries: 1 })
// M7-1: the model proposes, the background confirmer decides; anything left
// pending (review timeout, a refused proposal awaiting a person) is visible
// here and can be adopted or ignored without another model call.
const candidateList = useResource((signal) => getMemoryCandidates('pending', { signal }), {
  timeoutMs: 6000,
  retries: 1,
})
usePoll(() => {
  list.load()
  candidateList.load()
}, 15000)

const items = computed<MemoryItem[]>(() => list.data.value?.items ?? [])
const pending = computed<MemoryCandidate[]>(() => candidateList.data.value?.items ?? [])
const max = computed(() => list.data.value?.max ?? 200)
const maxText = computed(() => list.data.value?.max_text ?? 200)

const draftText = ref('')
const draftKind = ref('fact')
const editing = ref<Record<string, string>>({})
const confirmingClear = ref(false)
const { busy, message, tone, run } = useSave()

const kindOptions = computed(() => [
  { value: 'fact', label: t('memoryKindFact') },
  { value: 'preference', label: t('memoryKindPreference') },
  { value: 'event', label: t('memoryKindEvent') },
  { value: 'alias', label: t('memoryKindAlias') },
])

const kindLabel = (kind: string): string =>
  kindOptions.value.find((o) => o.value === kind)?.label ?? kind

function scopeLabel(item: { scope_person?: string; scope_device?: string }): string {
  return [item.scope_person ? `${t('chatScopePersonId')}: ${item.scope_person}` : '',
    item.scope_device ? `${t('chatScopeDeviceId')}: ${item.scope_device}` : ''].filter(Boolean).join(' · ')
    || t('chatScopeHousehold')
}

async function add(): Promise<void> {
  const text = draftText.value.trim()
  if (!text) return
  await run(async () => {
    await addMemory({ text, kind: draftKind.value })
    draftText.value = ''
    await list.load()
    return t('memorySaved')
  })
}

function editText(m: MemoryItem): string {
  return editing.value[m.id] ?? m.text
}

function setEditText(m: MemoryItem, value: string): void {
  editing.value = { ...editing.value, [m.id]: value }
}

async function saveEdit(m: MemoryItem): Promise<void> {
  const text = (editing.value[m.id] ?? m.text).trim()
  if (!text || text === m.text) {
    const next = { ...editing.value }
    delete next[m.id]
    editing.value = next
    return
  }
  await run(async () => {
    await updateMemory(m.id, { text })
    const next = { ...editing.value }
    delete next[m.id]
    editing.value = next
    await list.load()
    return t('memorySaved')
  })
}

async function togglePin(m: MemoryItem): Promise<void> {
  await run(async () => {
    await updateMemory(m.id, { pinned: !m.pinned })
    await list.load()
    return t('memorySaved')
  })
}

/** Single-item delete used to fire on the first click with no way back
 * (audit R2-23). There is no undo endpoint, so the confirmation carries the
 * text being deleted — that is the only chance to notice it is the wrong one. */
const confirmingDelete = ref<MemoryItem | null>(null)

async function remove(m: MemoryItem): Promise<void> {
  confirmingDelete.value = m
}

async function confirmRemove(): Promise<void> {
  const m = confirmingDelete.value
  confirmingDelete.value = null
  if (!m) return
  await run(async () => {
    await deleteMemory(m.id)
    await list.load()
    return t('memoryDeleted')
  })
}

async function clearAll(): Promise<void> {
  confirmingClear.value = false
  await run(async () => {
    const r = await clearMemories()
    await list.load()
    return t('memoryCleared', { n: r.deleted })
  })
}

async function adoptCandidate(c: MemoryCandidate): Promise<void> {
  await run(async () => {
    await confirmMemoryCandidate(c.id)
    await Promise.all([list.load(), candidateList.load()])
    return t('memoryCandidateAdopted')
  })
}

async function ignoreCandidate(c: MemoryCandidate): Promise<void> {
  await run(async () => {
    await rejectMemoryCandidate(c.id)
    await candidateList.load()
    return t('memoryCandidateIgnored')
  })
}
</script>

<template>
  <Panel
    icon="settings"
    apply="immediate"
    :title="t('memoryTitle')"
    :hint="t('memoryHint')"
  >
    <template #actions>
      <span class="memory__count mono">{{ items.length }} / {{ max }}</span>
    </template>

    <ErrorNotice v-if="list.error.value" :error="list.error.value" @retry="list.load" />
    <!-- Audit R1-4: this endpoint answers 200 with `{items: [], error}` when the
         store fails. An empty list would look like a household with no
         memories, so the error is shown instead. -->
    <p v-else-if="list.data.value?.error" class="memory__msg memory__msg--error">
      {{ t('memoryLoadFailed', { detail: list.data.value.error }) }}
    </p>
    <div v-else class="memory">
      <p class="memory__policy">{{ t('memoryPolicy') }}</p>

      <div class="memory__add">
        <TextInput v-model="draftText" :label="t('memoryAdd')" :hint="t('memoryAddHint')" />
        <Select v-model="draftKind" :label="t('memoryKind')" :options="kindOptions" />
        <button type="button" class="btn btn--primary" :disabled="busy || !draftText.trim()" @click="add">
          {{ t('memoryAddAction') }}
        </button>
      </div>

      <div v-if="pending.length" class="memory__candidates">
        <p class="memory__policy">{{ t('memoryCandidatesHint') }}</p>
        <ul class="memory__list">
          <li v-for="c in pending" :key="c.id" class="memory__item memory__item--candidate">
            <div class="memory__row">
              <span class="memory__text">{{ c.text }}</span>
              <span class="memory__kind mono">{{ kindLabel(c.kind) }}</span>
              <button type="button" class="btn btn--primary" :disabled="busy" @click="adoptCandidate(c)">
                {{ t('memoryCandidateAdopt') }}
              </button>
              <button type="button" class="btn" :disabled="busy" @click="ignoreCandidate(c)">
                {{ t('memoryCandidateIgnore') }}
              </button>
            </div>
            <p class="memory__source">{{ t('memoryVisibility') }}: {{ scopeLabel(c) }}</p>
          </li>
        </ul>
      </div>

      <EmptyState v-if="!items.length" :title="t('memoryEmpty')" />

      <ul v-else class="memory__list">
        <li v-for="m in items" :key="m.id" class="memory__item">
          <div class="memory__row">
            <input
              class="memory__text"
              :aria-label="t('memoryText')"
              :value="editText(m)"
              :maxlength="maxText"
              @input="setEditText(m, ($event.target as HTMLInputElement).value)"
              @blur="saveEdit(m)"
              @keydown.enter="($event.target as HTMLInputElement).blur()"
            />
            <span class="memory__kind mono">{{ kindLabel(m.kind) }}</span>
            <span class="memory__source mono">{{ m.source }}</span>
            <button type="button" class="btn" :disabled="busy" @click="togglePin(m)">
              {{ m.pinned ? t('memoryUnpin') : t('memoryPin') }}
            </button>
            <button type="button" class="btn btn--danger" :disabled="busy" @click="remove(m)">
              {{ t('memoryDelete') }}
            </button>
          </div>
          <p class="memory__source">{{ t('memoryVisibility') }}: {{ scopeLabel(m) }}</p>
        </li>
      </ul>

      <div v-if="items.length" class="memory__actions">
        <button type="button" class="btn btn--danger" :disabled="busy" @click="confirmingClear = true">
          {{ t('memoryClearAll') }}
        </button>
        <span v-if="message" class="memory__msg" :class="`memory__msg--${tone}`">{{ message }}</span>
      </div>
      <p v-else-if="message" class="memory__msg" :class="`memory__msg--${tone}`">{{ message }}</p>
    </div>

    <ConfirmDialog
      v-if="confirmingDelete"
      :title="t('memoryDelete')"
      :message="t('memoryDeleteConfirm', { text: confirmingDelete.text })"
      :confirm-label="t('memoryDelete')"
      :cancel-label="t('cancel')"
      tone="danger"
      @confirm="confirmRemove"
      @cancel="confirmingDelete = null"
    />

    <ConfirmDialog
      v-if="confirmingClear"
      :title="t('memoryClearAll')"
      :message="t('memoryClearConfirm')"
      :confirm-label="t('memoryClearAll')"
      :cancel-label="t('cancel')"
      tone="danger"
      @confirm="clearAll"
      @cancel="confirmingClear = false"
    />
  </Panel>
</template>

<style scoped>
.memory {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.memory__count {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.memory__policy {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.memory__add {
  display: grid;
  grid-template-columns: minmax(12rem, 2fr) minmax(8rem, 1fr) auto;
  gap: var(--sp-2);
  align-items: end;
}

.memory__list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  margin: 0;
  padding: 0;
  list-style: none;
}

.memory__item {
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  padding: var(--sp-2);
}

.memory__candidates {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.memory__item--candidate {
  border-style: dashed;
}

.memory__row {
  display: grid;
  grid-template-columns: minmax(10rem, 1fr) auto auto auto auto;
  gap: var(--sp-2);
  align-items: center;
}

.memory__text {
  width: 100%;
  padding: var(--sp-1) var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface);
  color: var(--ink);
  font: inherit;
}

.memory__kind,
.memory__source {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.memory__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
}

.memory__msg {
  font-size: var(--fs-sm);
}

.memory__msg--ok {
  color: var(--ok);
}

.memory__msg--danger {
  color: var(--danger);
}

@media (max-width: 900px) {
  .memory__add,
  .memory__row {
    grid-template-columns: 1fr;
    align-items: stretch;
  }
}
</style>
