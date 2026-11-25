<script setup lang="ts">
/**
 * Timed reminders (2026-11-14, user request).
 *
 * The voice path is the main one — 「十分钟后提醒我关火」 — and this card is
 * the same store seen from the console: what is pending, what already fired,
 * and a way to set or cancel one without talking. Every row's time is the
 * CORE's resolved time (the same `due_ms` the speaker will use); the card
 * never re-derives a time from the note text.
 *
 * A reminder belongs to the speaker that was told (M12a), so the card only
 * creates for the device it is showing and labels rows from other speakers
 * rather than hiding them.
 */
import { computed, onMounted, ref, watch } from 'vue'
import {
  addReminder,
  cancelReminder,
  getReminders,
  isDeviceRequiredError,
  type ModeDevice,
  type Reminder,
} from '../../api'
import { deviceName, devices as storeDevices } from '../../stores/devices'
import { t } from '../../i18n'
import { errorDetail, formatTime } from '../../format'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'
import ErrorNotice from '../ErrorNotice.vue'

const props = defineProps<{
  device: ModeDevice | null
  devices?: ModeDevice[]
}>()

const availableDevices = computed<ModeDevice[]>(() => {
  if (props.devices !== undefined) return props.devices
  if (storeDevices.value.length) return storeDevices.value
  return props.device ? [props.device] : []
})

const targetDevice = ref('')
watch(
  [() => props.device?.device, availableDevices],
  ([devId, list], [prevDevId]) => {
    const propChanged = prevDevId !== undefined && devId !== prevDevId
    if (devId && (propChanged || !targetDevice.value || !list.some((d) => d.device === targetDevice.value))) {
      targetDevice.value = devId
      return
    }
    if (targetDevice.value && list.some((d) => d.device === targetDevice.value)) {
      return
    }
    targetDevice.value = devId || (list.length === 1 ? list[0].device : '')
  },
  { immediate: true },
)

const rows = ref<Reminder[] | null>(null)
const error = ref<unknown>(null)
const loading = ref(false)
const busy = ref(false)
const message = ref('')
const tone = ref<'ok' | 'error'>('ok')

// New-reminder form. Relative (minutes) is the common case; the absolute
// shape asks only for a time today (or tomorrow when it already passed).
const mode = ref<'delay' | 'clock'>('delay')
const note = ref('')
const minutes = ref(10)
const hour = ref(8)
const minute = ref(0)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    const page = await getReminders()
    rows.value = page.reminders
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}

onMounted(() => void load())

const pending = computed(() => (rows.value ?? []).filter((r) => r.state === 'pending'))
const done = computed(() => (rows.value ?? []).filter((r) => r.state !== 'pending').slice(0, 8))

async function create(): Promise<void> {
  if (busy.value) return
  const target = targetDevice.value.trim() || props.device?.device?.trim() || ''
  if (!target) {
    message.value = availableDevices.value.length > 1 ? t('deviceRequiredError') : t('noDevice')
    tone.value = 'error'
    return
  }
  const text = note.value.trim()
  if (!text) {
    message.value = t('reminderNeedNote')
    tone.value = 'error'
    return
  }
  busy.value = true
  message.value = ''
  try {
    const body =
      mode.value === 'delay'
        ? { note: text, device: target, delay_minutes: minutes.value }
        : { note: text, device: target, hour: hour.value, minute: minute.value }
    await addReminder(body)
    note.value = ''
    tone.value = 'ok'
    message.value = t('reminderSaved')
    await load()
  } catch (e) {
    tone.value = 'error'
    message.value = isDeviceRequiredError(e) ? t('deviceRequiredError') : errorDetail(e)
  } finally {
    busy.value = false
  }
}

async function cancel(r: Reminder): Promise<void> {
  busy.value = true
  message.value = ''
  try {
    await cancelReminder(r.id)
    tone.value = 'ok'
    message.value = t('reminderCancelled')
    await load()
  } catch (e) {
    tone.value = 'error'
    message.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
  }
}

function stateLabel(r: Reminder): string {
  return r.state === 'pending'
    ? t('reminderPending')
    : r.state === 'fired'
      ? t('reminderFired')
      : t('reminderCancelledState')
}
</script>

<template>
  <Panel icon="reminder" :title="t('reminderTitle')" :hint="t('reminderHint')">
    <template #actions>
      <Pill v-if="targetDevice" size="sm" tone="accent" mono>
        {{ t('scopeThisDevice', { device: targetDevice }) }}
      </Pill>
      <button type="button" class="btn" :disabled="loading" @click="load">
        {{ loading ? t('loading') : t('refresh') }}
      </button>
    </template>

    <ErrorNotice v-if="error" :error="error" @retry="load" />

    <template v-else>
      <section class="rem">
        <div v-if="availableDevices.length > 1" class="rem__row">
          <label class="rem__label">
            <span>{{ t('reminderTargetDevice') }}</span>
            <select v-model="targetDevice" class="input mono" :aria-label="t('reminderTargetDevice')">
              <option value="" disabled>{{ t('selectTargetDevice') }}</option>
              <option v-for="d in availableDevices" :key="d.device" :value="d.device">
                {{ deviceName(d.device) === d.device ? d.device : `${deviceName(d.device)} (${d.device})` }}
              </option>
            </select>
          </label>
        </div>
        <div class="rem__row">
          <label class="rem__label">
            <input v-model="mode" type="radio" value="delay" />
            {{ t('reminderInMinutes') }}
          </label>
          <input
            v-model.number="minutes"
            class="input input--num"
            type="number"
            min="1"
            max="10080"
            :disabled="mode !== 'delay'"
            :aria-label="t('reminderInMinutes')"
          />
          <label class="rem__label">
            <input v-model="mode" type="radio" value="clock" />
            {{ t('reminderAtClock') }}
          </label>
          <input
            v-model.number="hour"
            class="input input--num"
            type="number"
            min="0"
            max="23"
            :disabled="mode !== 'clock'"
            :aria-label="t('reminderHour')"
          />
          <span class="mono">:</span>
          <input
            v-model.number="minute"
            class="input input--num"
            type="number"
            min="0"
            max="59"
            :disabled="mode !== 'clock'"
            :aria-label="t('reminderMinute')"
          />
        </div>
        <div class="rem__row">
          <input
            v-model="note"
            class="input input--wide"
            type="text"
            maxlength="40"
            :placeholder="t('reminderNotePlaceholder')"
            :aria-label="t('reminderNote')"
            @keyup.enter="create"
          />
          <button type="button" class="btn btn--primary" :disabled="busy || (!targetDevice && availableDevices.length <= 1)" @click="create">
            {{ busy ? t('saving') : t('reminderAdd') }}
          </button>
        </div>
        <p class="rem__hint">{{ t('reminderClockHint') }}</p>
        <p v-if="message" class="rem__msg" :class="`rem__msg--${tone}`">{{ message }}</p>
      </section>

      <EmptyState v-if="rows && !pending.length" :title="t('reminderEmpty')" />
      <table v-if="pending.length" class="rem__table">
        <thead>
          <tr>
            <th>{{ t('reminderWhen') }}</th>
            <th>{{ t('reminderNote') }}</th>
            <th>{{ t('reminderDevice') }}</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="r in pending" :key="r.id">
            <td class="mono">{{ formatTime(r.due_ms) }}</td>
            <td>{{ r.note }}</td>
            <td class="mono">
              <Pill v-if="targetDevice && r.device !== targetDevice" size="sm" tone="warn">{{ deviceName(r.device) === r.device ? r.device : `${deviceName(r.device)} (${r.device})` }}</Pill>
              <span v-else class="rem__same">{{ t('reminderThisDevice') }}</span>
            </td>
            <td>
              <button type="button" class="btn btn--small" :disabled="busy" @click="cancel(r)">
                {{ t('reminderCancel') }}
              </button>
            </td>
          </tr>
        </tbody>
      </table>

      <details v-if="done.length" class="rem__done">
        <summary>{{ t('reminderHistory') }}</summary>
        <ul>
          <li v-for="r in done" :key="r.id">
            <span class="mono">{{ formatTime(r.due_ms) }}</span>
            · {{ r.note }} ·
            <Pill size="sm" tone="neutral">{{ stateLabel(r) }}</Pill>
          </li>
        </ul>
      </details>
    </template>
  </Panel>
</template>

<style scoped>
.rem {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.rem__row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.rem__label {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  font-size: var(--fs-sm);
}

.rem__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
  max-width: 70ch;
}

.rem__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.rem__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.rem__table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-sm);
  margin-top: var(--sp-2);
}

.rem__table th {
  text-align: left;
  font-weight: 500;
  color: var(--muted);
  padding-bottom: var(--sp-1);
}

.rem__table td {
  padding: 0.28rem 0;
  border-top: 1px solid var(--line);
}

.rem__same {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.rem__done {
  margin-top: var(--sp-2);
  font-size: var(--fs-sm);
  color: var(--muted);
}

.rem__done ul {
  margin: var(--sp-1) 0 0;
  padding-left: var(--sp-4);
}

.input {
  border: 1px solid var(--line-strong);
  background: var(--surface);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.25rem var(--sp-2);
  font-size: var(--fs-sm);
}

.input--num {
  width: 5rem;
}

.input--wide {
  flex: 1 1 16rem;
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

.btn--small {
  padding: 0.1rem var(--sp-2);
  font-size: var(--fs-xs);
}

.btn--primary {
  border-color: var(--accent);
  background: var(--accent);
  color: var(--on-accent, #fff);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
