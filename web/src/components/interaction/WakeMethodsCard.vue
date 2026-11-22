<script setup lang="ts">
/**
 * Wake methods (2026-11-14, Codex-reviewed).
 *
 * The single most confusing thing in this console: one wake phrase can be
 * heard on two different paths, configured in two different editors.
 *   - 设备识别 / on-device: a fixed word compiled into the speaker's keyword
 *     file. ~300 ms, no transcript, no audio leaves the device.
 *   - 转写匹配 / transcript: the core matches the ASR text. ~1 s+, needs the
 *     ASR service, and is either enabled or switched off while idle.
 *
 * This card answers, per phrase, "is it actually reachable, and on which
 * path" by reading the live snapshot: the wake-word list, the command table's
 * wake-intent rows, the spotter's deployment/load proof, and the idle-ASR
 * switch. It configures nothing.
 */
import { computed } from 'vue'
import { t } from '../../i18n'
import type { ModeDevice, ModeSnapshot } from '../../api'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
}>()

interface Row {
  phrase: string
  transcript: 'idle_ok' | 'idle_off' | 'absent'
  device: 'loaded' | 'pending' | 'absent'
  summary: string
  tone: 'ok' | 'warn' | 'danger' | 'neutral'
}

const idleOff = computed(() => props.mode?.asr_wake_fallback === false)

const rows = computed<Row[]>(() => {
  const wakeWords = props.mode?.wake_words ?? []
  const reflex = props.device?.reflex
  const live = reflex?.words ?? []
  const deploy = reflex?.deploy ?? null
  // "Loaded" is a device fact: the spotter reported the hash it loaded and it
  // equals the installed file. A dirty or missing deploy counts as pending.
  const deviceLoaded = !!deploy && deploy.state === 'loaded' && deploy.loaded_matches === true

  const deviceWake = live.filter((w) => w.enabled && w.intent === 'wake').map((w) => w.text)

  // Union, ordered: the wake-phrase list first (that is the user's intent),
  // then wake-intent command words that are not in it.
  const phrases = [...wakeWords]
  for (const w of deviceWake) if (!phrases.includes(w)) phrases.push(w)

  return phrases.map((phrase) => {
    const inList = wakeWords.includes(phrase)
    const inTable = deviceWake.includes(phrase)
    const transcript = inList || inTable ? (idleOff.value ? 'idle_off' : 'idle_ok') : 'absent'
    const device = inTable ? (deviceLoaded ? 'loaded' : 'pending') : 'absent'
    const paths = (transcript !== 'absent' ? 1 : 0) + (device === 'loaded' ? 1 : 0)
    const summary =
      paths >= 2 ? 'wakePathBoth' : device === 'loaded' ? 'wakePathOnlyDevice' : transcript === 'idle_ok' ? 'wakePathOnlyTranscript' : 'wakePathNone'
    const tone = paths >= 2 ? 'ok' : transcript === 'idle_ok' || device === 'loaded' ? 'warn' : 'danger'
    return { phrase, transcript, device, summary, tone }
  })
})
</script>

<template>
  <Panel icon="wake" :title="t('wakeMethodsTitle')" :hint="t('wakeMethodsHint')">
    <p v-if="!rows.length" class="wm__empty">{{ t('wakeMethodsEmpty') }}</p>
    <div v-else class="wm__table-wrap">
      <table class="wm__table">
        <thead>
          <tr>
            <th>{{ t('wakeTitle') }}</th>
            <th>{{ t('wakePathDevice') }}</th>
            <th>{{ t('wakePathTranscript') }}</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="row in rows" :key="row.phrase">
            <td class="wm__phrase">{{ row.phrase }}</td>
            <td>
              <Pill size="sm" :tone="row.device === 'loaded' ? 'ok' : row.device === 'pending' ? 'warn' : 'neutral'">
                {{ row.device === 'loaded' ? t('wakePathLoaded') : row.device === 'pending' ? t('wakePathPending') : t('wakePathMissing') }}
              </Pill>
            </td>
            <td>
              <Pill size="sm" :tone="row.transcript === 'idle_ok' ? 'ok' : row.transcript === 'idle_off' ? 'warn' : 'neutral'">
                {{ row.transcript === 'idle_ok' ? t('wakePathIdleOk') : row.transcript === 'idle_off' ? t('wakePathIdleOff') : t('wakePathMissing') }}
              </Pill>
            </td>
            <td>
              <Pill size="sm" :tone="row.tone">{{ t(row.summary as never) }}</Pill>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </Panel>
</template>

<style scoped>
.wm__empty {
  color: var(--muted);
  font-size: var(--fs-sm);
}

.wm__table-wrap {
  overflow-x: auto;
  max-width: 100%;
}

.wm__table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-sm);
}

.wm__table th {
  text-align: left;
  font-weight: 500;
  color: var(--muted);
  padding-bottom: var(--sp-1);
  white-space: nowrap;
}

.wm__table td {
  padding: 0.3rem 0;
  border-top: 1px solid var(--line);
}

.wm__phrase {
  font-family: var(--font-mono, monospace);
}
</style>
