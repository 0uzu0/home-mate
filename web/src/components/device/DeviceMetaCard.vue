<script setup lang="ts">
/**
 * Household labels for one device: a display name and a room. These are
 * console-owned metadata (stored by the core, included in backup/history),
 * not firmware state — renaming a device never changes its stable id, its
 * connection, or its voice settings.
 */
import { nextTick, ref, useId, watch } from 'vue'
import { putDeviceMeta, type DeviceMeta, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import TextInput from '../TextInput.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = defineProps<{
  device: ModeDevice | null
  meta: DeviceMeta | undefined
  metaLoaded: boolean
  metaError: unknown
  rooms: string[]
  refresh: () => void | Promise<void>
}>()

const { busy, message, tone, run } = useSave()
const name = ref('')
const room = ref('')
const roomListId = useId()
// True once the user edits either field. The 2 s store poll must not refill
// the form underneath an edit (clearing both fields is a legitimate save).
const dirty = ref(false)
let loading = false

function load(): void {
  loading = true
  name.value = props.meta?.name ?? ''
  room.value = props.meta?.room ?? ''
  void nextTick(() => {
    loading = false
    dirty.value = false
  })
}

watch([name, room], () => {
  if (!loading) dirty.value = true
})

watch(
  () => props.device?.device,
  () => load(),
  { immediate: true },
)
watch(
  () => props.meta,
  () => {
    // A clean form follows the latest server value (another browser may have
    // changed it); an edited form keeps its draft.
    if (props.metaLoaded && !busy.value && !dirty.value) load()
  },
  { immediate: true },
)
watch(
  () => props.metaLoaded,
  (loaded) => {
    if (loaded && !busy.value && !dirty.value) load()
  },
)

async function save(): Promise<void> {
  const target = props.device?.device
  if (!target) return
  await run(async () => {
    const result = await putDeviceMeta(target, {
      name: name.value.trim(),
      room: room.value.trim(),
    })
    // The user may have switched devices while the request was in flight; do
    // not paint A's saved label into B's form.
    if (props.device?.device === target) {
      loading = true
      name.value = result.meta.name
      room.value = result.meta.room
      void nextTick(() => {
        loading = false
        dirty.value = false
      })
    }
    await props.refresh()
  })
}
</script>

<template>
  <Panel icon="device" :title="t('deviceMetaTitle')" :hint="t('deviceMetaHint')" apply="immediate">
    <EmptyState v-if="!device" :title="t('noDevice')" />
    <ErrorNotice
      v-else-if="!metaLoaded && metaError"
      :error="metaError"
      compact
      @retry="refresh"
    />
    <EmptyState v-else-if="!metaLoaded" :title="t('deviceMetaLoading')" />
    <template v-else>
      <div class="devmeta__grid">
        <TextInput
          v-model="name"
          :label="t('deviceNameLabel')"
          :hint="t('deviceNameHint')"
          :placeholder="t('deviceNamePlaceholder')"
          :maxlength="40"
          wide
        />
        <TextInput
          v-model="room"
          :label="t('deviceRoomLabel')"
          :hint="t('deviceRoomHint')"
          :placeholder="t('deviceRoomPlaceholder')"
          :maxlength="40"
          :list="roomListId"
          wide
        />
        <datalist :id="roomListId">
          <option v-for="item in rooms" :key="item" :value="item" />
        </datalist>
      </div>
      <p class="devmeta__note">{{ t('deviceMetaNote') }}</p>
      <div class="devmeta__actions">
        <button type="button" class="btn btn--primary" :disabled="busy || !metaLoaded" @click="save">
          {{ busy ? t('saving') : t('save') }}
        </button>
        <span v-if="message" class="devmeta__msg" :class="`devmeta__msg--${tone}`">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.devmeta__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: var(--sp-3);
  align-items: start;
}

.devmeta__note {
  font-size: var(--fs-xs);
  color: var(--muted);
  max-width: 70ch;
}

.devmeta__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.24rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn--primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.devmeta__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.devmeta__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
