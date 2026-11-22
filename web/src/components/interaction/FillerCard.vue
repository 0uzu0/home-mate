<script setup lang="ts">
/**
 * Waiting cues. Per-scene lists; an empty scene is OFF (it never borrows
 * another scene). Radio vs interrupt is the cue's relationship to music.
 */
import { computed, ref, watch } from 'vue'
import { putMode, type FillerMode, type FillerScene, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import PhraseList from '../PhraseList.vue'
import Slider from '../Slider.vue'
import Select from '../Select.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

type Scenes = Record<FillerScene, string[]>

interface Draft {
  delay: number
  /** R2: once the model asks for a tool, the cue may be this soon. */
  toolDelay: number
  mode: FillerMode
  scenes: Scenes
}

const draft = ref<Draft | null>(null)
const { busy, message, tone, run } = useSave()

const sceneKeys: FillerScene[] = ['thinking', 'search', 'device', 'story']
/** Computed, not frozen at setup: switching the language while the page is
 * mounted used to leave these scene names in the previous one
 * (audit 2026-11-14). */
const sceneLabels = computed<Record<FillerScene, string>>(() => ({
  thinking: t('fillerSceneThinking'),
  search: t('fillerSceneSearch'),
  device: t('fillerSceneDevice'),
  story: t('fillerSceneStory'),
}))

watch(
  () => props.mode,
  (mode) => {
    if (mode?.filler && draft.value === null) {
      draft.value = {
        delay: mode.filler.delay_ms,
        toolDelay: mode.filler.tool_delay_ms ?? 600,
        mode: (mode.filler.mode as FillerMode) || 'radio',
        scenes: {
          thinking: [...mode.filler.thinking],
          search: [...mode.filler.search],
          device: [...mode.filler.device],
          story: [...mode.filler.story],
        },
      }
    }
  },
  { immediate: true },
)

const modeOptions = computed(() => [
  { value: 'radio', label: t('fillerModeRadio') },
  { value: 'interrupt', label: t('fillerModeInterrupt') },
])

async function save(resynth = false): Promise<void> {
  if (!props.device || !draft.value) return
  const current = draft.value
  await run(async () => {
    await putMode(
      props.device!.device,
      {
        filler: {
          delay_ms: current.delay,
          tool_delay_ms: current.toolDelay,
          mode: current.mode,
          thinking: current.scenes.thinking,
          search: current.scenes.search,
          device: current.scenes.device,
          story: current.scenes.story,
          ...(resynth ? { resynth: true } : {}),
        },
      },
      { timeoutMs: 12000 },
    )
    await props.refresh()
    return resynth ? t('resynthQueued') : undefined
  })
}
</script>

<template>
  <Panel
    icon="filler" apply="immediate" :title="t('fillerTitle')" :hint="t('fillerHint')">
    <template #actions>
      <span v-if="mode?.filler" class="filler__current mono">
        {{ t('current') }}: {{ mode.filler.delay_ms }} ms · {{ mode.filler.tool_delay_ms ?? 600 }} ms · {{ mode.filler.mode || 'radio' }}
      </span>
    </template>

    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else-if="draft">
      <div class="filler__controls">
        <Slider
          v-model="draft.delay"
          :label="t('fillerDelay')"
          :min="200"
          :max="10000"
          :step="100"
          :format="(value) => `${value} ms`"
          :hint="t('fillerDelayUnit')"
        />
        <Slider
          v-model="draft.toolDelay"
          :label="t('fillerToolDelay')"
          :min="0"
          :max="5000"
          :step="100"
          :format="(value) => `${value} ms`"
          :hint="t('fillerToolDelayUnit')"
        />
        <Select v-model="draft.mode" :label="t('fillerMode')" :options="modeOptions" />
      </div>

      <div class="filler__scenes">
        <div v-for="scene in sceneKeys" :key="scene" class="filler__scene">
          <PhraseList
            v-model="draft.scenes[scene]"
            :label="sceneLabels[scene]"
            :placeholder="t('fillerPlaceholder')"
            :maxlength="40"
            :disabled="busy"
            :empty-label="t('fillerOff')"
          />
        </div>
      </div>

      <div class="filler__live">
        <span v-for="item in mode?.devices ?? []" :key="item.device" class="filler__live-item">
          <span class="mono">{{ item.device }}</span>
          <span v-if="item.filler?.live" class="filler__live-on">
            {{ t('fillerLive') }}: {{ item.filler.live.phrase }} ({{ item.filler.live.scene }})
          </span>
          <span v-else class="filler__live-off">{{ t('fillerIdle') }}</span>
        </span>
      </div>

      <div class="filler__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save()">
          {{ busy ? t('saving') : t('fillerSave') }}
        </button>
        <button type="button" class="btn" :disabled="busy" @click="save(true)">
          {{ t('resynth') }}
        </button>
        <span v-if="message" class="filler__msg" :class="`filler__msg--${tone}`">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.filler__current {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.filler__controls {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 14rem), 1fr));
  gap: var(--sp-4);
  align-items: start;
}

.filler__scenes {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr));
  gap: var(--sp-3);
}

.filler__scene {
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.filler__live {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}

.filler__live-item {
  display: flex;
  gap: var(--sp-3);
  align-items: baseline;
  font-size: var(--fs-xs);
  color: var(--muted);
}

.filler__live-on {
  color: var(--live);
}

.filler__live-off {
  color: var(--muted);
}

.filler__actions {
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

.filler__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.filler__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
