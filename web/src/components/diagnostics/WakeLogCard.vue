<script setup lang="ts">
/**
 * Wake log (2026-11-14).
 *
 * The user's ask: "when a conversation starts, I want to see WHICH path woke
 * it and WHICH word" — because the fix for a false wake is usually to remove
 * or re-spell a single word, and until now that meant reading raw events.
 *
 * Two paths produce a session opener:
 *   - 设备识别 / on-device: `keyword_wake` (nothing was playing) or
 *     `keyword_stop` (it stopped playback, i.e. a wake over content);
 *   - 转写匹配 / transcript: `mic_wake` (the transcript carried a wake word).
 * `mic_ignored_unwoken` is shown separately: speech that was transcribed but
 * did not wake — the near misses that make a word look "too eager".
 *
 * Reads the same journal as the acceptance tools; nothing is fabricated and a
 * failure is shown as a failure.
 */
import { computed, onMounted, ref } from 'vue'
import { getVoiceEvents, type VoiceEvent } from '../../api'
import { t } from '../../i18n'
import { formatTime } from '../../format'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'

const HOURS = 24
const LIMIT = 800

const events = ref<VoiceEvent[] | null>(null)
const error = ref<unknown>(null)
const loading = ref(false)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    const page = await getVoiceEvents(Date.now() - HOURS * 3600_000, LIMIT, { tail: true })
    events.value = page.events
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}

onMounted(() => void load())

interface Opening {
  ts: number
  path: 'device' | 'transcript'
  word: string
  text?: string
  stopped?: boolean
}

const openings = computed<Opening[]>(() => {
  const out: Opening[] = []
  for (const e of events.value ?? []) {
    if (e.kind === 'keyword_wake' || e.kind === 'keyword_stop') {
      out.push({
        ts: e.ts_ms,
        path: 'device',
        word: String(e.keyword ?? '—'),
        stopped: e.kind === 'keyword_stop',
      })
    } else if (e.kind === 'mic_wake') {
      out.push({
        ts: e.ts_ms,
        path: 'transcript',
        word: String(e.wake ?? '—'),
        text: typeof e.text === 'string' ? e.text : undefined,
      })
    }
  }
  return out.sort((a, b) => b.ts - a.ts).slice(0, 40)
})

/** Per word × path counts over the window — the tuning view. */
const byWord = computed(() => {
  const map = new Map<string, { device: number; transcript: number }>()
  for (const o of openings.value) {
    const row = map.get(o.word) ?? { device: 0, transcript: 0 }
    row[o.path] += 1
    map.set(o.word, row)
  }
  return [...map.entries()]
    .map(([word, c]) => ({ word, ...c, total: c.device + c.transcript }))
    .sort((a, b) => b.total - a.total)
})

const nearMisses = computed(() =>
  (events.value ?? [])
    .filter((e) => e.kind === 'mic_ignored_unwoken')
    .sort((a, b) => (b.ts_ms ?? 0) - (a.ts_ms ?? 0))
    .slice(0, 12),
)
</script>

<template>
  <Panel icon="kws" :title="t('wakeLogTitle')" :hint="t('wakeLogHint')">
    <template #actions>
      <Pill size="sm" tone="neutral">{{ t('wakeLogWindow') }}</Pill>
      <button type="button" class="btn" :disabled="loading" @click="load">
        {{ loading ? t('loading') : t('refresh') }}
      </button>
    </template>

    <ErrorNotice v-if="error" :error="error" @retry="load" />
    <EmptyState v-else-if="!events" :title="t('loading')" />
    <template v-else>
      <EmptyState v-if="!openings.length" :title="t('wakeLogEmpty')" />
      <template v-else>
        <table class="wl__table">
          <thead>
            <tr>
              <th>{{ t('wakeLogWhen') }}</th>
              <th>{{ t('wakeLogPath') }}</th>
              <th>{{ t('wakeLogWord') }}</th>
              <th>{{ t('wakeLogDetail') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(o, i) in openings" :key="i">
              <td class="mono">{{ formatTime(o.ts) }}</td>
              <td>
                <Pill size="sm" :tone="o.path === 'device' ? 'ok' : 'warn'">
                  {{ o.path === 'device' ? t('wakePathDevice') : t('wakePathTranscript') }}
                </Pill>
              </td>
              <td class="mono">{{ o.word }}</td>
              <td class="wl__detail">
                {{ o.stopped ? t('wakeLogStopped') : o.text ? `「${o.text}」` : '' }}
              </td>
            </tr>
          </tbody>
        </table>

        <div class="wl__counts">
          <span class="wl__label">{{ t('wakeLogByWord') }}</span>
          <span v-for="w in byWord" :key="w.word" class="mono wl__count">
            {{ w.word }} · {{ t('wakePathDevice') }} {{ w.device }} · {{ t('wakePathTranscript') }} {{ w.transcript }}
          </span>
        </div>
      </template>

      <details v-if="nearMisses.length" class="wl__near">
        <summary>{{ t('wakeLogNearMiss') }}</summary>
        <ul>
          <li v-for="(e, i) in nearMisses" :key="i" class="mono">
            {{ formatTime(e.ts_ms) }} · 「{{ e.text }}」
          </li>
        </ul>
      </details>
    </template>
  </Panel>
</template>

<style scoped>
.wl__table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-sm);
}

.wl__table th {
  text-align: left;
  font-weight: 500;
  color: var(--muted);
  padding-bottom: var(--sp-1);
}

.wl__table td {
  padding: 0.28rem 0;
  border-top: 1px solid var(--line);
  vertical-align: top;
}

.wl__detail {
  color: var(--muted);
}

.wl__counts {
  margin-top: var(--sp-2);
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wl__label {
  color: var(--ink);
}

.wl__count {
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  padding: 0.1rem var(--sp-2);
}

.wl__near {
  margin-top: var(--sp-2);
  font-size: var(--fs-sm);
  color: var(--muted);
}

.wl__near ul {
  margin: var(--sp-1) 0 0;
  padding-left: var(--sp-4);
}
</style>
