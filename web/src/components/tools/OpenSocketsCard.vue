<script setup lang="ts">
/**
 * #22 Open Sockets (Declarative Socket Registry).
 *
 * Lists registered external HTTP tools and lets the owner create, update,
 * toggle, or delete declarative socket contracts with explicit Query/Mutate
 * side-effect and JSON parameter schema.
 */
import { computed, ref } from 'vue'
import {
  createOpenSocket,
  deleteOpenSocket,
  getOpenSockets,
  updateOpenSocket,
  type OpenSocketMethod,
  type OpenSocketStatus,
  type SocketSideEffect,
} from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import ErrorNotice from '../ErrorNotice.vue'
import EmptyState from '../EmptyState.vue'
import ConfirmDialog from '../ConfirmDialog.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import Select from '../Select.vue'
import TextInput from '../TextInput.vue'
import NumberField from '../NumberField.vue'
import PasswordField from '../PasswordField.vue'

const list = useResource((signal) => getOpenSockets({ signal }), { timeoutMs: 6000, retries: 1 })

const sockets = computed<OpenSocketStatus[]>(() => list.data.value?.sockets ?? [])

interface SocketDraft {
  editingName: string | null
  name: string
  service: string
  description: string
  method: OpenSocketMethod
  effect: SocketSideEffect
  endpointUrl: string
  timeoutMs: number
  enabled: boolean
  authHeader: string
  token: string
  tokenSet: boolean
  parametersJson: string
}

const DEFAULT_SCHEMA = '{\n  "type": "object",\n  "properties": {}\n}'

function emptyDraft(): SocketDraft {
  return {
    editingName: null,
    name: '',
    service: 'open_socket',
    description: '',
    method: 'GET',
    effect: 'Query',
    endpointUrl: '',
    timeoutMs: 10000,
    enabled: true,
    authHeader: '',
    token: '',
    tokenSet: false,
    parametersJson: DEFAULT_SCHEMA,
  }
}

const draft = ref<SocketDraft | null>(null)
const formError = ref('')
const confirmingDelete = ref<OpenSocketStatus | null>(null)
const { busy, message, tone, run } = useSave()
// A status poll cannot supersede the authoritative post-save readback.
usePoll(() => { if (!busy.value) return list.load() }, 15000)

const effectOptions = computed(() => [
  { value: 'Query', label: t('openSocketsEffectQuery') },
  { value: 'Mutate', label: t('openSocketsEffectMutate') },
])

const methodOptions = computed(() => {
  const base: Array<{ value: OpenSocketMethod; label: string }> = [
    { value: 'GET', label: 'GET' },
    { value: 'POST', label: 'POST' },
  ]
  if (draft.value?.effect === 'Mutate') {
    base.push(
      { value: 'PUT', label: 'PUT' },
      { value: 'PATCH', label: 'PATCH' },
      { value: 'DELETE', label: 'DELETE' },
    )
  }
  return base
})

function startCreate(): void {
  formError.value = ''
  draft.value = emptyDraft()
}

function startEdit(s: OpenSocketStatus): void {
  formError.value = ''
  draft.value = {
    editingName: s.name,
    name: s.name,
    service: s.service || 'open_socket',
    description: s.description || '',
    method: s.method,
    effect: s.effect,
    endpointUrl: s.endpoint_url,
    timeoutMs: s.timeout_ms ?? 10000,
    enabled: s.enabled,
    authHeader: s.auth_header || '',
    token: '',
    tokenSet: s.token_set,
    parametersJson: JSON.stringify(s.parameters ?? { type: 'object', properties: {} }, null, 2),
  }
}

function cancelEdit(): void {
  formError.value = ''
  draft.value = null
}

async function saveSocket(): Promise<void> {
  if (!draft.value) return
  const d = draft.value
  formError.value = ''

  const name = d.name.trim()
  const description = d.description.trim()
  const endpointUrl = d.endpointUrl.trim()
  if (!name || !description || !endpointUrl) {
    formError.value = t('openSocketsRequiredFields')
    return
  }

  let parsedParams: Record<string, unknown>
  try {
    const raw = JSON.parse(d.parametersJson.trim() || '{}')
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
      formError.value = t('openSocketsParamsInvalid')
      return
    }
    parsedParams = raw as Record<string, unknown>
  } catch {
    formError.value = t('openSocketsParamsInvalid')
    return
  }

  const body = {
    name,
    service: d.service.trim() || 'open_socket',
    description,
    method: d.method,
    effect: d.effect,
    endpoint_url: endpointUrl,
    timeout_ms: d.timeoutMs,
    enabled: d.enabled,
    auth_header: d.authHeader.trim(),
    parameters: parsedParams,
    ...(d.token.trim() ? { token: d.token.trim() } : {}),
  }

  await run(async () => {
    if (d.editingName) {
      await updateOpenSocket(d.editingName, body)
    } else {
      await createOpenSocket(body)
    }
    await list.load()
    if (list.error.value) throw list.error.value
    draft.value = null
    return t('openSocketsSaved', { name })
  })
}

async function confirmRemove(): Promise<void> {
  const target = confirmingDelete.value
  confirmingDelete.value = null
  if (!target) return
  await run(async () => {
    await deleteOpenSocket(target.name)
    if (draft.value?.editingName === target.name) {
      draft.value = null
    }
    await list.load()
    return t('openSocketsDeleted', { name: target.name })
  })
}
</script>

<template>
  <Panel
    icon="json"
    apply="next-turn"
    :title="t('openSocketsTitle')"
    :hint="t('openSocketsHint')"
  >
    <template #actions>
      <Pill size="sm" mono>{{ t('openSocketsCount', { n: sockets.length }) }}</Pill>
      <button type="button" class="btn btn--primary" @click="startCreate">
        <AppIcon name="add" :size="13" /> {{ t('openSocketsNew') }}
      </button>
    </template>

    <ErrorNotice v-if="list.error.value" :error="list.error.value" compact @retry="list.load()" />

    <div v-if="draft" class="os__form">
      <h3 class="os__form-title">
        {{
          draft.editingName
            ? t('openSocketsFormTitleEdit', { name: draft.editingName })
            : t('openSocketsFormTitleCreate')
        }}
      </h3>

      <div class="os__grid">
        <TextInput
          v-model="draft.name"
          :label="t('openSocketsName')"
          :placeholder="t('openSocketsNamePlaceholder')"
          :disabled="Boolean(draft.editingName)"
        />
        <TextInput
          v-model="draft.service"
          :label="t('openSocketsService')"
          :placeholder="t('openSocketsServicePlaceholder')"
        />
        <Select v-model="draft.effect" :label="t('openSocketsEffect')" :options="effectOptions" />
        <Select v-model="draft.method" :label="t('openSocketsMethod')" :options="methodOptions" />
        <TextInput
          v-model="draft.endpointUrl"
          :label="t('openSocketsEndpoint')"
          :placeholder="t('openSocketsEndpointPlaceholder')"
          wide
        />
        <TextInput
          v-model="draft.description"
          :label="t('openSocketsDescription')"
          :placeholder="t('openSocketsDescriptionPlaceholder')"
          wide
        />
        <NumberField v-model="draft.timeoutMs" :label="t('openSocketsTimeout')" :min="500" :max="30000" />
        <TextInput
          v-model="draft.authHeader"
          :label="t('openSocketsAuthHeader')"
          :placeholder="t('openSocketsAuthHeaderPlaceholder')"
        />
        <PasswordField
          v-model="draft.token"
          :label="t('openSocketsToken')"
          :is-set="draft.tokenSet"
          :hint="t('openSocketsTokenHint')"
        />
      </div>

      <Toggle v-model="draft.enabled" :label="t('openSocketsEnabled')" />

      <div class="os__schema">
        <label class="os__schema-label">{{ t('openSocketsParams') }}</label>
        <textarea v-model="draft.parametersJson" class="os__textarea mono" rows="5" />
      </div>

      <p v-if="formError" class="os__msg os__msg--error">{{ formError }}</p>

      <div class="os__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="saveSocket">
          <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
        </button>
        <button type="button" class="btn" :disabled="busy" @click="cancelEdit">
          {{ t('openSocketsCancelEdit') }}
        </button>
      </div>
    </div>

    <EmptyState v-if="!sockets.length" :title="t('openSocketsEmpty')" />

    <ul v-else class="os__list">
      <li v-for="s in sockets" :key="s.name" class="os__item">
        <div class="os__item-head">
          <span class="mono os__name">{{ s.name }}</span>
          <Pill size="sm" mono>{{ s.service }}</Pill>
          <Pill size="sm" mono>{{ s.method }}</Pill>
          <Pill :tone="s.effect === 'Mutate' ? 'warn' : 'info'" size="sm" mono>
            {{ s.effect === 'Mutate' ? t('openSocketsEffectMutate') : t('openSocketsEffectQuery') }}
          </Pill>
          <Pill :tone="s.enabled ? 'ok' : 'warn'" size="sm">
            {{ s.enabled ? t('openSocketsEnabledPill') : t('openSocketsDisabledPill') }}
          </Pill>
        </div>
        <div class="os__item-meta">
          <span class="mono os__url">{{ s.endpoint_url }}</span>
          <span class="os__desc">{{ s.description }}</span>
        </div>
        <div class="os__item-actions">
          <button type="button" class="btn" :disabled="busy" @click="startEdit(s)">
            {{ t('openSocketsEdit') }}
          </button>
          <button type="button" class="btn btn--danger" :disabled="busy" @click="confirmingDelete = s">
            {{ t('openSocketsDelete') }}
          </button>
        </div>
      </li>
    </ul>

    <p v-if="message" class="os__msg" :class="`os__msg--${tone}`">{{ message }}</p>

    <ConfirmDialog
      v-if="confirmingDelete"
      :title="t('openSocketsDelete')"
      :message="t('openSocketsDeleteConfirm', { name: confirmingDelete.name })"
      :confirm-label="t('openSocketsDelete')"
      :cancel-label="t('cancel')"
      tone="danger"
      @confirm="confirmRemove"
      @cancel="confirmingDelete = null"
    />
  </Panel>
</template>

<style scoped>
.os__form {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-md);
  background: var(--surface-2);
}

.os__form-title {
  font-size: var(--fs-md);
  margin: 0;
}

.os__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-3);
}

.os__schema {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.os__schema-label {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.os__textarea {
  width: 100%;
  padding: var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--bg);
  color: var(--ink);
  font-size: var(--fs-sm);
  resize: vertical;
}

.os__actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: center;
}

.os__list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  margin: 0;
  padding: 0;
  list-style: none;
}

.os__item {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.os__item-head {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: center;
}

.os__name {
  font-weight: 600;
}

.os__item-meta {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  font-size: var(--fs-sm);
}

.os__url {
  color: var(--muted);
  word-break: break-all;
}

.os__item-actions {
  display: flex;
  gap: var(--sp-2);
}

.os__msg {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.os__msg--ok {
  color: var(--ok);
}

.os__msg--error {
  color: var(--danger);
}
</style>
