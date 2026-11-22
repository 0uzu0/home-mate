<script setup lang="ts">
/**
 * W4b (#83): Governed `Agent.md` (versioned + 1-click rollback, Console-only)
 * & Auditable `USER.md` (provenance-tagged profile entries + Console edit/revert).
 */
import { computed, onMounted, ref, watch } from 'vue'
import {
  getAgentMd,
  getUserMd,
  putAgentMd,
  putUserMd,
  revertUserMd,
  rollbackAgentMd,
  type AgentMdRevision,
  type UserMdEntry,
} from '../../api'
import { t } from '../../i18n'
import { useResource } from '../../hooks/useResource'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import TextInput from '../TextInput.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'
import LoadingBlock from '../LoadingBlock.vue'

const agentRes = useResource((signal) => getAgentMd({ signal }), { timeoutMs: 6000, retries: 1 })
const userRes = useResource((signal) => getUserMd('all', { signal }), { timeoutMs: 6000, retries: 1 })

const agentContent = ref('')
const agentNote = ref('')
const agentTouched = ref(false)
const agentReady = computed(() => agentRes.data.value !== null && !agentRes.error.value && !agentRes.loading.value)
const userReady = computed(() => userRes.data.value !== null && !userRes.error.value && !userRes.loading.value)
onMounted(() => Promise.all([agentRes.load(), userRes.load()]))

watch(
  () => agentRes.data.value,
  (snap) => {
    if (!snap || agentTouched.value) return
    agentContent.value = snap.content ?? ''
  },
  { immediate: true },
)

const revisions = computed<AgentMdRevision[]>(() => agentRes.data.value?.revisions ?? [])
const currentVersion = computed(() => agentRes.data.value?.version ?? 0)
const entries = computed<UserMdEntry[]>(() => userRes.data.value?.entries ?? [])

const editEntryId = ref<string | undefined>(undefined)
const userPersonId = ref('')
const userKey = ref('')
const userContent = ref('')
const userRationale = ref('')

const { busy, message, tone, run } = useSave()

function formatTime(ms: number): string {
  if (!ms) return '—'
  return new Date(ms).toLocaleString()
}

async function reloadAgentMd(): Promise<void> {
  await agentRes.load()
  if (!agentReady.value) return
  agentContent.value = agentRes.data.value!.content
  agentNote.value = ''
  agentTouched.value = false
}

async function saveAgentMd(): Promise<void> {
  if (!agentReady.value) return
  await run(async () => {
    const saved = await putAgentMd({
      content: agentContent.value,
      note: agentNote.value.trim(),
    })
    await agentRes.load()
    if (agentRes.error.value || agentRes.data.value?.version !== saved.version || agentRes.data.value?.content !== saved.content) throw new Error(t('configReadbackFailed'))
    agentContent.value = saved.content
    agentNote.value = ''
    agentTouched.value = false
    return t('agentMdSaved')
  })
}

async function rollbackTo(rev: AgentMdRevision): Promise<void> {
  if (!agentReady.value) return
  await run(async () => {
    const saved = await rollbackAgentMd({ id: rev.id })
    await agentRes.load()
    if (agentRes.error.value || agentRes.data.value?.version !== saved.version || agentRes.data.value?.content !== saved.content) throw new Error(t('configReadbackFailed'))
    agentContent.value = saved.content
    agentNote.value = ''
    agentTouched.value = false
    return t('agentMdRolledBack', { v: rev.version })
  })
}

function startEditUser(entry: UserMdEntry): void {
  if (!userReady.value) return
  editEntryId.value = entry.id
  userPersonId.value = entry.person_id
  userKey.value = entry.key
  userContent.value = entry.content
  userRationale.value = entry.rationale
}

function clearUserForm(): void {
  editEntryId.value = undefined
  userPersonId.value = ''
  userKey.value = ''
  userContent.value = ''
  userRationale.value = ''
}

async function saveUserEntry(): Promise<void> {
  const key = userKey.value.trim()
  const content = userContent.value.trim()
  if (!userReady.value || !key || !content) return
  await run(async () => {
    await putUserMd({
      ...(editEntryId.value ? { id: editEntryId.value } : {}),
      person_id: userPersonId.value.trim(),
      key,
      content,
      rationale: userRationale.value.trim(),
    })
    clearUserForm()
    await userRes.load()
    return t('userMdSaved')
  })
}

async function revertEntry(entry: UserMdEntry): Promise<void> {
  if (!userReady.value) return
  await run(async () => {
    await revertUserMd(entry.id)
    await userRes.load()
    return t('userMdReverted')
  })
}
</script>

<template>
  <Panel
    icon="settings"
    apply="next-turn"
    :title="t('agentPersonaTitle')"
    :hint="t('agentPersonaHint')"
  >
    <template #actions>
      <span class="mono ap__ver">v{{ currentVersion }}</span>
    </template>

    <div class="ap">
      <section class="ap__section">
        <h3 class="ap__heading">{{ t('agentMdHeading') }}</h3>
        <p class="ap__sub">{{ t('agentMdSub') }}</p>
        <ErrorNotice v-if="agentRes.error.value" :error="agentRes.error.value" @retry="agentRes.load" />

        <LoadingBlock v-if="agentRes.loading.value && !agentRes.data.value" :label="t('loading')" />
        <p v-if="agentTouched" role="status">{{ t('configDraftKept') }}</p>
        <textarea
          v-model="agentContent"
          :aria-label="t('agentMdHeading')"
          class="ap__textarea"
          rows="6"
          maxlength="16000"
          :placeholder="t('agentMdPlaceholder')"
          :disabled="busy || !agentReady"
          @input="agentTouched = true"
        />
        <div class="ap__row">
          <TextInput
            v-model="agentNote"
            :label="t('agentMdNoteLabel')"
            :placeholder="t('agentMdNotePlaceholder')"
            :disabled="busy || !agentReady"
            @update:model-value="agentTouched = true"
            wide
          />
          <button
            type="button"
            class="btn btn--primary"
            :disabled="busy || !agentReady"
            @click="saveAgentMd"
          >
            {{ t('agentMdSave') }}
          </button>
          <button type="button" class="btn" :disabled="busy || agentRes.loading.value" @click="reloadAgentMd">{{ t('configReloadDraft') }}</button>
          <span v-if="message" class="ap__msg" :class="`ap__msg--${tone}`">{{ message }}</span>
        </div>

        <details v-if="revisions.length" class="ap__history">
          <summary>{{ t('agentMdRevisions', { n: revisions.length }) }}</summary>
          <ul class="ap__rev-list">
            <li v-for="rev in revisions" :key="rev.id" class="ap__rev">
              <div class="ap__rev-meta">
                <span class="mono">v{{ rev.version }}</span>
                <Pill size="sm">{{ rev.author }}</Pill>
                <span v-if="rev.note" class="ap__note">{{ rev.note }}</span>
                <span class="ap__time mono">{{ formatTime(rev.created_ms) }}</span>
              </div>
              <button
                type="button"
                class="btn"
                :disabled="busy || !agentReady || rev.version === currentVersion"
                @click="rollbackTo(rev)"
              >
                {{ t('agentMdRollback') }}
              </button>
            </li>
          </ul>
        </details>
      </section>

      <section class="ap__section">
        <h3 class="ap__heading">{{ t('userMdHeading') }}</h3>
        <p class="ap__sub">{{ t('userMdSub') }}</p>
        <ErrorNotice v-if="userRes.error.value" :error="userRes.error.value" @retry="userRes.load" />

        <LoadingBlock v-if="userRes.loading.value && !userRes.data.value" :label="t('loading')" />
        <fieldset class="ap__form" :disabled="busy || !userReady">
          <div class="ap__form-grid">
            <TextInput
              v-model="userPersonId"
              :label="t('userMdPerson')"
              :placeholder="t('userMdPersonPlaceholder')"
            />
            <TextInput
              v-model="userKey"
              :label="t('userMdKey')"
              :placeholder="t('userMdKeyPlaceholder')"
            />
            <TextInput
              v-model="userContent"
              :label="t('userMdContent')"
              :placeholder="t('userMdContentPlaceholder')"
              wide
            />
            <TextInput
              v-model="userRationale"
              :label="t('userMdRationale')"
              :placeholder="t('userMdRationalePlaceholder')"
              wide
            />
          </div>
          <div class="ap__row">
            <button
              type="button"
              class="btn btn--primary"
              :disabled="busy || !userReady || !userKey.trim() || !userContent.trim()"
              @click="saveUserEntry"
            >
              {{ editEntryId ? t('userMdUpdate') : t('userMdAdd') }}
            </button>
            <button v-if="editEntryId" type="button" class="btn" :disabled="busy" @click="clearUserForm">
              {{ t('cancel') }}
            </button>
          </div>
        </fieldset>

        <EmptyState v-if="userReady && !entries.length" :title="t('userMdEmpty')" />
        <ul v-else class="ap__entry-list">
          <li
            v-for="entry in entries"
            :key="entry.id"
            class="ap__entry"
            :class="{ 'ap__entry--reverted': entry.status === 'reverted' }"
          >
            <div class="ap__entry-head">
              <span class="mono ap__entry-key">{{ entry.key }}</span>
              <Pill v-if="entry.person_id" size="sm" mono>{{ entry.person_id }}</Pill>
              <Pill v-if="entry.scope_device" size="sm" mono>{{ t('chatScopeDeviceId') }}: {{ entry.scope_device }}</Pill>
              <Pill v-if="!entry.person_id && !entry.scope_device" size="sm" mono>{{ t('workingMemoryGlobalScope') }}</Pill>
              <Pill size="sm" :tone="entry.status === 'active' ? 'ok' : 'warn'">
                {{ entry.status === 'active' ? t('userMdActive') : t('userMdRevertedBadge') }}
              </Pill>
              <Pill size="sm">{{ entry.author }}</Pill>
              <div class="ap__entry-btns">
                <button type="button" class="btn" :disabled="busy || !userReady" @click="startEditUser(entry)">
                  {{ t('workingMemoryEdit') }}
                </button>
                <button
                  v-if="entry.status === 'active'"
                  type="button"
                  class="btn btn--danger"
                  :disabled="busy || !userReady"
                  @click="revertEntry(entry)"
                >
                  {{ t('userMdRevert') }}
                </button>
              </div>
            </div>
            <p class="ap__entry-content">{{ entry.content }}</p>
            <p class="ap__entry-prov mono">
              {{ t('userMdProvenance', {
                device: entry.source_device || '—',
                trace: entry.trace_id || '—',
                time: formatTime(entry.turn_ts),
                rationale: entry.rationale || '—',
              }) }}
            </p>
          </li>
        </ul>
      </section>
    </div>
  </Panel>
</template>

<style scoped>
.ap {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.ap__ver {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.ap__section {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding-top: var(--sp-2);
  border-top: 1px solid var(--line);
}

.ap__section:first-child {
  padding-top: 0;
  border-top: none;
}

.ap__heading {
  font-size: var(--fs-md);
  margin: 0;
}

.ap__sub {
  font-size: var(--fs-sm);
  color: var(--muted);
  margin: 0;
}

.ap__textarea {
  width: 100%;
  box-sizing: border-box;
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--bg);
  color: var(--ink);
  font: inherit;
  padding: var(--sp-2);
  resize: vertical;
}

.ap__row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: end;
}

.ap__msg {
  font-size: var(--fs-sm);
}

.ap__msg--ok {
  color: var(--ok);
}

.ap__msg--error {
  color: var(--danger);
}

.ap__history summary {
  cursor: pointer;
  font-size: var(--fs-sm);
  color: var(--muted);
}

.ap__rev-list,
.ap__entry-list {
  list-style: none;
  padding: 0;
  margin: var(--sp-2) 0 0;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.ap__rev,
.ap__entry {
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  padding: var(--sp-2);
  background: var(--surface-2);
}

.ap__rev {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.ap__rev-meta,
.ap__entry-head {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.ap__note,
.ap__time {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.ap__form {
  border: 0;
  min-width: 0;
  margin: 0;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.ap__form-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
  gap: var(--sp-2);
}

.ap__entry--reverted {
  opacity: 0.6;
}

.ap__entry-btns {
  margin-left: auto;
  display: flex;
  gap: var(--sp-1);
}

.ap__entry-content {
  margin: var(--sp-1) 0;
}

.ap__entry-prov {
  margin: 0;
  font-size: var(--fs-xs);
  color: var(--muted);
  word-break: break-all;
}
</style>
