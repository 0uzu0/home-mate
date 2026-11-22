<script setup lang="ts">
/** Wake words: the pinyin-matched phrases that open the follow window. */
import { computed, ref, watch } from 'vue'
import { putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import PhraseList from '../PhraseList.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

/** The ASR fallback being off means these words can only match inside an
 * open session (idle speech is not transcribed at all), so the card must say
 * which path it is actually on instead of letting the user assume "fast". */
const asrOff = computed(() => props.mode?.asr_wake_fallback === false)
const draft = ref<string[] | null>(null)
const { busy, message, tone, run } = useSave()

watch(
  () => props.mode,
  (mode) => {
    if (mode && draft.value === null) draft.value = [...(mode.wake_words ?? [])]
  },
  { immediate: true },
)

async function save(): Promise<void> {
  if (!props.device || !draft.value) return
  const words = draft.value
  await run(async () => {
    await putMode(props.device!.device, { wake_words: words })
    await props.refresh()
  })
}
</script>

<template>
  <Panel
    icon="wake" apply="immediate" :title="t('wakeTitle')" :hint="t('wakeHint')">
    <template #actions>
      <Pill size="sm" :tone="asrOff ? 'warn' : 'neutral'">
        {{ asrOff ? t('wakePathAsrOff') : t('wakePathAsr') }}
      </Pill>
      <span v-if="mode" class="wake__current mono">
        {{ t('current') }}: {{ mode.wake_words.join(' / ') || '—' }}
      </span>
    </template>

    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else-if="draft">
      <PhraseList
        v-model="draft"
        :label="t('wakeTitle')"
        :placeholder="t('wakePlaceholder')"
        :maxlength="16"
        :disabled="busy"
        :empty-label="t('wakeEmpty')"
      />
      <p class="wake__path">{{ t('wakeLocalHint') }}</p>
      <div class="wake__actions">
        <button type="button" class="btn btn--primary" :disabled="busy || !draft.length" @click="save">
          {{ busy ? t('saving') : t('wakeSave') }}
        </button>
        <span v-if="message" class="wake__msg" :class="`wake__msg--${tone}`">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.wake__path {
  color: var(--muted);
  font-size: var(--fs-sm);
}

.wake__current {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wake__actions {
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

.wake__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.wake__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
