<script setup lang="ts">
/**
 * How much the assistant may SAY on a voice turn.
 *
 * Since 2026-11-14 the MODEL chooses the shape from the tiered prompt (short /
 * explanation / long content); these two tiers are soft targets written into
 * the system prompt. The runtime keeps only a runaway BACKSTOP far above both
 * — a story runs whole, and an ordinary answer is whatever length the model
 * judged right. The rest of any cut answer is always kept, and 「继续」 reads
 * on. Applies from the next turn (no restart).
 */
import { ref, watch } from 'vue'
import { putMode, type ModeDevice, type ModeSnapshot } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import NumberField from '../NumberField.vue'
import SectionLabel from '../SectionLabel.vue'
import EmptyState from '../EmptyState.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  refresh: () => void | Promise<void>
}>()

const sentences = ref<number | null>(null)
const chars = ref<number | null>(null)
const detailSentences = ref<number | null>(null)
const detailChars = ref<number | null>(null)
const backstopSentences = ref<number | null>(null)
const backstopChars = ref<number | null>(null)
const continueHint = ref('')
const resumeLead = ref('')
const { busy, message, tone, run } = useSave()

watch(
  () => props.mode?.spoken,
  (spoken) => {
    if (!spoken || sentences.value !== null) return
    sentences.value = spoken.max_sentences
    chars.value = spoken.max_chars
    detailSentences.value = spoken.detail_max_sentences
    detailChars.value = spoken.detail_max_chars
    backstopSentences.value = spoken.backstop_max_sentences
    backstopChars.value = spoken.backstop_max_chars
    continueHint.value = spoken.continue_hint
    resumeLead.value = spoken.resume_lead ?? ''
  },
  { immediate: true },
)

async function save(): Promise<void> {
  if (
    !props.device ||
    sentences.value === null ||
    chars.value === null ||
    detailSentences.value === null ||
    detailChars.value === null ||
    backstopSentences.value === null ||
    backstopChars.value === null
  )
    return
  await run(async () => {
    await putMode(props.device!.device, {
      spoken: {
        max_sentences: sentences.value!,
        max_chars: chars.value!,
        detail_max_sentences: detailSentences.value!,
        detail_max_chars: detailChars.value!,
        backstop_max_sentences: backstopSentences.value!,
        backstop_max_chars: backstopChars.value!,
        continue_hint: continueHint.value.trim(),
        resume_lead: resumeLead.value.trim(),
      },
    })
    await props.refresh()
  })
}
</script>

<template>
  <Panel icon="spoken" apply="immediate" :title="t('spokenTitle')" :hint="t('spokenHint')">
    <EmptyState v-if="!device" :title="t('noDevice')" />

    <template v-else>
      <section class="spoken">
        <SectionLabel :text="t('spokenTitle')" />
        <div class="spoken__row">
          <NumberField
            v-if="sentences !== null"
            v-model="sentences"
            :label="t('spokenSentences')"
            :min="1"
            :max="8"
          />
          <NumberField
            v-if="chars !== null"
            v-model="chars"
            :label="t('spokenChars')"
            :min="40"
            :max="600"
            :unit="t('unitChars')"
          />
        </div>
      </section>

      <section class="spoken">
        <SectionLabel :text="t('spokenDetailWords')" />
        <p class="spoken__hint">{{ t('spokenDetailWordsHint') }}</p>
        <div class="spoken__row">
          <NumberField
            v-if="detailSentences !== null"
            v-model="detailSentences"
            :label="t('spokenDetailSentences')"
            :min="1"
            :max="8"
          />
          <NumberField
            v-if="detailChars !== null"
            v-model="detailChars"
            :label="t('spokenDetailChars')"
            :min="40"
            :max="600"
            :unit="t('unitChars')"
          />
        </div>
      </section>

      <section class="spoken">
        <SectionLabel :text="t('spokenBackstop')" />
        <p class="spoken__hint">{{ t('spokenBackstopHint') }}</p>
        <div class="spoken__row">
          <NumberField
            v-if="backstopSentences !== null"
            v-model="backstopSentences"
            :label="t('spokenBackstopSentences')"
            :min="2"
            :max="1000"
          />
          <NumberField
            v-if="backstopChars !== null"
            v-model="backstopChars"
            :label="t('spokenBackstopChars')"
            :min="200"
            :max="6000"
            :unit="t('unitChars')"
          />
        </div>
      </section>

      <section class="spoken">
        <SectionLabel :text="t('spokenContinueHint')" />
        <p class="spoken__hint">{{ t('spokenContinueHintHint') }}</p>
        <input v-model="continueHint" type="text" class="input" spellcheck="false" :aria-label="t('spokenContinueHint')" />
      </section>

      <section class="spoken">
        <SectionLabel :text="t('spokenResumeLead')" />
        <p class="spoken__hint">{{ t('spokenResumeLeadHint') }}</p>
        <input v-model="resumeLead" type="text" class="input" spellcheck="false" :aria-label="t('spokenResumeLead')" />
      </section>

      <!-- Audit R2-18: one save for the whole card, at the end and sticky.
           It used to sit after the first two fields, before three more sections
           it also saves. -->
      <div class="spoken__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          {{ busy ? t('saving') : t('spokenSave') }}
        </button>
        <span v-if="message" class="spoken__msg" :class="`spoken__msg--${tone}`">{{ message }}</span>
      </div>
    </template>
  </Panel>
</template>

<style scoped>
.spoken {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.spoken__row {
  display: flex;
  align-items: end;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.spoken__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
  position: sticky;
  bottom: 0;
  z-index: 1;
  background: var(--surface);
  padding: var(--sp-2) 0;
  border-top: 1px solid var(--line);
}

.spoken__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
  max-width: 70ch;
}

.input {
  border: 1px solid var(--line-strong);
  background: var(--surface);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.25rem var(--sp-2);
  font-size: var(--fs-sm);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.22rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.spoken__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.spoken__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}
</style>
