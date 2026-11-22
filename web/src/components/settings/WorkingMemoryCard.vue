<script setup lang="ts">
/**
 * #23 Long-Term Working Memory management card.
 *
 * Completely separate from the 200-item household notes in `MemoryCard.vue`.
 * Stores project paths, active progress, and multi-session work context keyed
 * by `(scope_person, scope_device, key_or_topic)` with up to 4,000 chars per item.
 */
import { computed, ref, watch } from 'vue'
import {
  clearSharedMemoryOutbox,
  clearWorkingMemory,
  deleteWorkingMemoryById,
  getSharedMemoryStatus,
  getWorkingMemory,
  pinWorkingMemory,
  putSharedMemoryConfig,
  putWorkingMemory,
  type WorkingMemoryItem,
} from '../../api'
import { t } from '../../i18n'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import TextInput from '../TextInput.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'
import ConfirmDialog from '../ConfirmDialog.vue'
import Pill from '../Pill.vue'

const filterPerson = ref('')
const filterDevice = ref('')

const list = useResource(
  (signal) => getWorkingMemory(filterPerson.value, filterDevice.value, { signal }),
  { timeoutMs: 6000, retries: 1 },
)
const sharedStatus = useResource((signal) => getSharedMemoryStatus({ signal }), {
  timeoutMs: 6000,
  retries: 1,
})
usePoll(() => Promise.all([list.load(), sharedStatus.load()]).then(() => {}), 15000)

const items = computed<WorkingMemoryItem[]>(() => list.data.value?.items ?? [])
const max = computed(() => list.data.value?.max ?? 500)
const pinnedCount = computed(
  () => list.data.value?.pinned_count ?? items.value.filter((i) => i.pinned).length,
)
const evictedTotal = computed(() => list.data.value?.evicted_total ?? 0)
const capacityPct = computed(() =>
  Math.min(100, Math.round(((list.data.value?.count ?? items.value.length) / Math.max(1, max.value)) * 100)),
)
const maxKeyLen = computed(() => list.data.value?.max_key_len ?? 120)
const maxContentLen = computed(() => list.data.value?.max_content_len ?? 4000)

const sharedEnabled = ref(false)
const sharedUploadEnabled = ref(false)
const sharedNasUrl = ref('')
const sharedToken = ref('')
const sharedTouched = ref(false)
const sharedDirty = computed(() => sharedTouched.value || sharedToken.value.length > 0)
const sharedReady = computed(() => sharedStatus.data.value !== null && !sharedStatus.error.value && !sharedStatus.loading.value)
watch(
  () => sharedStatus.data.value,
  (s) => {
    if (!s || sharedDirty.value) return
    sharedEnabled.value = s.enabled
    sharedUploadEnabled.value = s.upload_enabled
    sharedNasUrl.value = s.nas_base_url
  },
  { immediate: true },
)

const draftPerson = ref('')
const draftDevice = ref('')
const draftKey = ref('')
const draftContent = ref('')
const formError = ref('')

const confirmingDelete = ref<WorkingMemoryItem | null>(null)
const confirmingClear = ref(false)
const { busy, message, tone, run } = useSave()

function applyFilter(): void {
  void list.load()
}

function resetFilter(): void {
  filterPerson.value = ''
  filterDevice.value = ''
  void list.load()
}

function editItem(item: WorkingMemoryItem): void {
  draftPerson.value = item.scope_person
  draftDevice.value = item.scope_device
  draftKey.value = item.key_or_topic
  draftContent.value = item.content
  formError.value = ''
}

async function saveEntry(): Promise<void> {
  const key = draftKey.value.trim()
  const content = draftContent.value.trim()
  formError.value = ''
  if (!key || !content) {
    formError.value = t('workingMemoryRequired')
    return
  }
  await run(async () => {
    await putWorkingMemory({
      scope_person: draftPerson.value.trim(),
      scope_device: draftDevice.value.trim(),
      key_or_topic: key,
      content,
    })
    draftKey.value = ''
    draftContent.value = ''
    await list.load()
    return t('workingMemorySaved')
  })
}

async function confirmRemove(): Promise<void> {
  const target = confirmingDelete.value
  confirmingDelete.value = null
  if (!target) return
  await run(async () => {
    await deleteWorkingMemoryById(target.id)
    await list.load()
    return t('workingMemoryDeleted')
  })
}

async function clearAll(): Promise<void> {
  confirmingClear.value = false
  await run(async () => {
    const r = await clearWorkingMemory(filterPerson.value, filterDevice.value)
    await list.load()
    return t('workingMemoryCleared', { n: r.deleted })
  })
}

async function togglePin(item: WorkingMemoryItem): Promise<void> {
  await run(async () => {
    await pinWorkingMemory(item.id, !item.pinned)
    await list.load()
    return t('workingMemorySaved')
  })
}

async function reloadSharedConfig(): Promise<void> {
  await sharedStatus.load()
  if (!sharedReady.value) return
  const s = sharedStatus.data.value!
  sharedEnabled.value = s.enabled
  sharedUploadEnabled.value = s.upload_enabled
  sharedNasUrl.value = s.nas_base_url
  sharedToken.value = ''
  sharedTouched.value = false
}

async function saveSharedConfig(): Promise<void> {
  if (!sharedReady.value) return
  await run(async () => {
    const trimmedToken = sharedToken.value.trim()
    const expected = { enabled: sharedEnabled.value, upload_enabled: sharedUploadEnabled.value, nas_base_url: sharedNasUrl.value.trim() }
    await putSharedMemoryConfig({
      enabled: sharedEnabled.value,
      upload_enabled: sharedUploadEnabled.value,
      nas_base_url: sharedNasUrl.value.trim(),
      ...(trimmedToken ? { token: trimmedToken } : {}),
    })
    await sharedStatus.load()
    const actual = sharedStatus.data.value
    if (sharedStatus.error.value || !actual || actual.enabled !== expected.enabled || actual.upload_enabled !== expected.upload_enabled || actual.nas_base_url !== expected.nas_base_url) throw new Error(t('configReadbackFailed'))
    sharedNasUrl.value = actual.nas_base_url
    sharedToken.value = ''
    sharedTouched.value = false
    return t('sharedMemoryConfigSaved')
  })
}

async function clearOutbox(): Promise<void> {
  await run(async () => {
    const r = await clearSharedMemoryOutbox()
    await sharedStatus.load()
    return t('sharedMemoryOutboxCleared', { n: r.deleted })
  })
}

function scopeLabel(item: WorkingMemoryItem): string {
  const person = item.scope_person || t('workingMemoryGlobalScope')
  const device = item.scope_device || t('workingMemoryGlobalScope')
  return `${person} · ${device}`
}
</script>

<template>
  <Panel
    icon="backup"
    apply="immediate"
    :title="t('workingMemoryTitle')"
    :hint="t('workingMemoryHint')"
  >
    <template #actions>
      <span class="wm__count mono">{{ items.length }} / {{ max }}</span>
    </template>

    <ErrorNotice v-if="list.error.value" :error="list.error.value" @retry="list.load" />
    <p v-else-if="list.data.value?.error" class="wm__msg wm__msg--error">
      {{ t('workingMemoryLoadFailed', { detail: list.data.value.error }) }}
    </p>

    <div v-else class="wm">
      <div class="wm__capacity">
        <div class="wm__capacity-head">
          <span class="mono">{{ t('workingMemoryCapacity', { count: items.length, max, pinned: pinnedCount }) }}</span>
          <span v-if="evictedTotal > 0" class="wm__evict-notice">
            {{ t('workingMemoryEvictionNotice', { n: evictedTotal }) }}
          </span>
        </div>
        <div class="wm__bar" aria-hidden="true">
          <div class="wm__bar-fill" :style="{ width: `${capacityPct}%` }" />
        </div>
      </div>

      <div class="wm__filter">
        <TextInput
          v-model="filterPerson"
          :label="t('workingMemoryFilterPerson')"
          :placeholder="t('workingMemoryScopePersonPlaceholder')"
          @submit="applyFilter"
        />
        <TextInput
          v-model="filterDevice"
          :label="t('workingMemoryFilterDevice')"
          :placeholder="t('workingMemoryScopeDevicePlaceholder')"
          @submit="applyFilter"
        />
        <button type="button" class="btn" @click="applyFilter">{{ t('workingMemoryFilterApply') }}</button>
        <button type="button" class="btn" @click="resetFilter">{{ t('workingMemoryFilterClear') }}</button>
      </div>

      <div class="wm__form">
        <div class="wm__form-grid">
          <TextInput
            v-model="draftPerson"
            :label="t('workingMemoryScopePerson')"
            :placeholder="t('workingMemoryScopePersonPlaceholder')"
          />
          <TextInput
            v-model="draftDevice"
            :label="t('workingMemoryScopeDevice')"
            :placeholder="t('workingMemoryScopeDevicePlaceholder')"
          />
          <TextInput
            v-model="draftKey"
            :label="t('workingMemoryKey')"
            :placeholder="t('workingMemoryKeyPlaceholder')"
            :maxlength="maxKeyLen"
            wide
          />
        </div>

        <div class="wm__content-wrap">
          <label class="wm__label">{{ t('workingMemoryContent') }}</label>
          <textarea
            v-model="draftContent"
            class="wm__textarea"
            rows="3"
            :maxlength="maxContentLen"
            :placeholder="t('workingMemoryContentPlaceholder')"
          />
          <span class="wm__chars mono">{{ draftContent.length }} / {{ maxContentLen }}</span>
        </div>

        <p v-if="formError" class="wm__msg wm__msg--error">{{ formError }}</p>

        <div class="wm__actions">
          <button
            type="button"
            class="btn btn--primary"
            :disabled="busy || !draftKey.trim() || !draftContent.trim()"
            @click="saveEntry"
          >
            {{ t('workingMemorySaveAction') }}
          </button>
          <span v-if="message" class="wm__msg" :class="`wm__msg--${tone}`">{{ message }}</span>
        </div>
      </div>

      <EmptyState v-if="!items.length" :title="t('workingMemoryEmpty')" />

      <ul v-else class="wm__list">
        <li v-for="item in items" :key="item.id" class="wm__item">
          <div class="wm__item-head">
            <span class="mono wm__key">{{ item.key_or_topic }}</span>
            <Pill size="sm" mono>{{ scopeLabel(item) }}</Pill>
            <Pill v-if="item.pinned" size="sm" tone="accent">{{ t('workingMemoryPinnedBadge') }}</Pill>
            <div class="wm__item-btns">
              <button type="button" class="btn" :disabled="busy" @click="togglePin(item)">
                {{ item.pinned ? t('workingMemoryUnpin') : t('workingMemoryPin') }}
              </button>
              <button type="button" class="btn" :disabled="busy" @click="editItem(item)">
                {{ t('workingMemoryEdit') }}
              </button>
              <button type="button" class="btn btn--danger" :disabled="busy" @click="confirmingDelete = item">
                {{ t('workingMemoryDelete') }}
              </button>
            </div>
          </div>
          <pre class="wm__content">{{ item.content }}</pre>
        </li>
      </ul>

      <div v-if="items.length" class="wm__actions">
        <button type="button" class="btn btn--danger" :disabled="busy" @click="confirmingClear = true">
          {{ t('workingMemoryClearAll') }}
        </button>
      </div>

      <div class="wm__shared">
        <div class="wm__shared-head">
          <span class="wm__shared-title">{{ t('sharedMemoryTitle') }}</span>
          <span class="mono wm__count">
            {{ t('sharedMemoryBacklog', { n: sharedStatus.data.value?.backlog_count ?? 0 }) }}
          </span>
          <Pill v-if="sharedStatus.data.value?.circuit_open" size="sm" tone="warn">
            {{ t('sharedMemoryCircuitOpen') }}
          </Pill>
        </div>
        <ErrorNotice v-if="sharedStatus.error.value" :error="sharedStatus.error.value" :stale="!!sharedStatus.data.value" :at="sharedStatus.updatedAt.value" @retry="sharedStatus.load" />
        <p v-if="sharedDirty" role="status">{{ t('configDraftKept') }}</p>
        <fieldset class="wm__shared-grid" :disabled="busy || !sharedReady">
          <label class="wm__check">
            <input v-model="sharedEnabled" type="checkbox" @change="sharedTouched = true" />
            <span>{{ t('sharedMemoryEnabled') }}</span>
          </label>
          <label class="wm__check">
            <input v-model="sharedUploadEnabled" type="checkbox" @change="sharedTouched = true" />
            <span>{{ t('sharedMemoryUploadEnabled') }}</span>
          </label>
          <TextInput
            v-model="sharedNasUrl"
            @update:model-value="sharedTouched = true"
            :label="t('sharedMemoryNasUrl')"
            :placeholder="t('sharedMemoryNasUrlPlaceholder')"
            wide
          />
          <TextInput
            v-model="sharedToken"
            type="password"
            :label="t('sharedMemoryToken')"
            :hint="sharedStatus.data.value?.token_set ? t('sharedMemoryTokenSetHint') : t('sharedMemoryTokenUnsetHint')"
            :placeholder="t('sharedMemoryTokenPlaceholder')"
            wide
          />
        </fieldset>
        <div class="wm__actions">
          <button type="button" class="btn btn--primary" :disabled="busy || !sharedReady" @click="saveSharedConfig">
            {{ t('sharedMemorySaveConfig') }}
          </button>
          <button type="button" class="btn" :disabled="busy || sharedStatus.loading.value" @click="reloadSharedConfig">{{ t('configReloadDraft') }}</button>
          <button
            type="button"
            class="btn"
            :disabled="busy || !(sharedStatus.data.value?.backlog_count)"
            @click="clearOutbox"
          >
            {{ t('sharedMemoryClearOutbox') }}
          </button>
        </div>
      </div>
    </div>

    <ConfirmDialog
      v-if="confirmingDelete"
      :title="t('workingMemoryDelete')"
      :message="t('workingMemoryDeleteConfirm', { key: confirmingDelete.key_or_topic })"
      :confirm-label="t('workingMemoryDelete')"
      :cancel-label="t('cancel')"
      tone="danger"
      @confirm="confirmRemove"
      @cancel="confirmingDelete = null"
    />

    <ConfirmDialog
      v-if="confirmingClear"
      :title="t('workingMemoryClearAll')"
      :message="t('workingMemoryClearConfirm')"
      :confirm-label="t('workingMemoryClearAll')"
      :cancel-label="t('cancel')"
      tone="danger"
      @confirm="clearAll"
      @cancel="confirmingClear = false"
    />
  </Panel>
</template>

<style scoped>
.wm {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.wm__count {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wm__filter {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: end;
}

.wm__form {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-md);
  background: var(--surface-2);
}

.wm__form-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
  gap: var(--sp-2);
}

.wm__content-wrap {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.wm__label {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.wm__textarea {
  width: 100%;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--bg);
  color: var(--ink);
  font-size: var(--fs-sm);
  font-family: inherit;
  resize: vertical;
}

.wm__chars {
  align-self: flex-end;
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wm__actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: center;
}

.wm__list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  margin: 0;
  padding: 0;
  list-style: none;
}

.wm__item {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.wm__item-head {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: center;
}

.wm__key {
  font-weight: 600;
}

.wm__item-btns {
  margin-left: auto;
  display: flex;
  gap: var(--sp-2);
}

.wm__content {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
  font-family: inherit;
  font-size: var(--fs-sm);
}

.wm__msg {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.wm__msg--ok {
  color: var(--ok);
}

.wm__msg--error {
  color: var(--danger);
}

.wm__capacity {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.wm__capacity-head {
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  gap: var(--sp-2);
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wm__evict-notice {
  color: var(--warn, var(--danger));
}

.wm__bar {
  height: 0.4rem;
  border-radius: 999px;
  background: var(--line);
  overflow: hidden;
}

.wm__bar-fill {
  height: 100%;
  background: var(--accent);
  transition: width 0.2s ease;
}

.wm__shared {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-md);
  background: var(--surface-2);
}

.wm__shared-head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
}

.wm__shared-title {
  font-weight: 600;
  font-size: var(--fs-sm);
}

.wm__shared-grid {
  border: 0;
  padding: 0;
  margin: 0;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.wm__check {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-sm);
}
</style>
