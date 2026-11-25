<script setup lang="ts">
/**
 * The one maintenance action that is not a config save: restart the core.
 *
 * Everyday settings apply immediately (each card carries an apply badge);
 * this exists for a stuck config, a post-upgrade restart, or an explicit
 * request. The page reconnects through the normal status poll.
 */
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { factoryReset, getMode, getStatus, restartCore } from '../../api'
import { clearAuthToken, markUnauthorized } from '../../auth'
import {
  deviceName,
  devices,
  selected,
  selectedId,
} from '../../stores/devices'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import { advancedMode } from '../../stores/advanced'
import Panel from '../Panel.vue'
import AppIcon from '../AppIcon.vue'
import ConfirmDialog from '../ConfirmDialog.vue'

const props = defineProps<{ deviceId?: string | null }>()
const emit = defineEmits<{ reset: [] }>()
const confirming = ref(false)
const confirmingReset = ref(false)
const requested = ref(false)
const { busy, message, tone, run } = useSave()
let timer: number | null = null
let epochBefore: number | null = null

const targetDeviceId = ref('')
const preferredDeviceId = computed(() => {
  const fromProp = props.deviceId?.trim()
  if (fromProp) return fromProp
  return selected.value?.device ?? selectedId.value ?? ''
})

watch(
  [devices, preferredDeviceId],
  ([list, pref], [, prevPref]) => {
    const prefChanged = prevPref !== undefined && pref !== prevPref
    const currentValid =
      Boolean(targetDeviceId.value) &&
      (!list.length || list.some((d) => d.device === targetDeviceId.value))
    if (pref && (!list.length || list.some((d) => d.device === pref)) && (prefChanged || !currentValid)) {
      targetDeviceId.value = pref
      return
    }
    if (currentValid) {
      return
    }
    targetDeviceId.value = list.length > 0 ? list[0].device : ''
  },
  { immediate: true },
)

function clearTimer(): void {
  if (timer !== null) {
    window.clearTimeout(timer)
    timer = null
  }
}

/**
 * The core is usually back before the 10 s status poll can observe an outage,
 * so recovery is confirmed by a short poll of our own: give the supervisor a
 * few seconds, then require the connection to be up.
 */
async function currentEpoch(): Promise<number | null> {
  try {
    const dev = targetDeviceId.value.trim() || undefined
    const mode = await getMode(dev ? { device: dev, timeoutMs: 2000 } : { timeoutMs: 2000 })
    const matched = dev
      ? mode.devices.find((d) => d.device === dev)
      : mode.devices.find((d) => typeof d.kws?.core_epoch === 'number')
    return matched?.kws?.core_epoch ?? null
  } catch {
    return null
  }
}

/**
 * The core must be a NEW process, not merely answering: compare the voice
 * pipeline epoch captured before the request. If the epoch is unavailable,
 * a successful status probe is the fallback.
 */
async function coreIsRestarted(): Promise<boolean> {
  if (epochBefore != null) {
    const epoch = await currentEpoch()
    if (epoch == null) return false
    // A known pre-restart epoch must CHANGE. The old fall-through treated an
    // unchanged process as recovered whenever the status probe answered
    // (audit 2026-11-14).
    return epoch !== epochBefore
  }
  try {
    await getStatus({ timeoutMs: 2000 })
    return true
  } catch {
    return false
  }
}

function pollRecovery(attempt = 0): void {
  clearTimer()
  timer = window.setTimeout(() => {
    void (async () => {
      if (!requested.value) return
      // Probe the core itself: the Settings status poll is 10 s and may still
      // hold the pre-restart value, which would claim recovery too early.
      if (attempt >= 1 && (await coreIsRestarted())) {
        requested.value = false
        message.value = t('restartRecovered')
        tone.value = 'ok'
        return
      }
      if (attempt < 20) {
        pollRecovery(attempt + 1)
      } else {
        // Stop waiting and unlock the button; silence here would look like a
        // restart that is still in progress forever.
        requested.value = false
        message.value = t('restartTimeout')
        tone.value = 'error'
      }
    })()
  }, 1200)
}

async function restart(): Promise<void> {
  try {
    await run(async () => {
      epochBefore = await currentEpoch()
      await restartCore()
      requested.value = true
      pollRecovery()
      return t('restarting')
    })
  } finally {
    confirming.value = false
  }
}

async function runFactoryReset(): Promise<void> {
  try {
    await run(async () => {
      await factoryReset('FACTORY_RESET')
      clearAuthToken()
      markUnauthorized()
      emit('reset')
      return t('factoryResetDone')
    })
  } finally {
    confirmingReset.value = false
  }
}

onBeforeUnmount(clearTimer)
</script>

<template>
  <Panel icon="power" :title="t('maintenanceTitle')" :hint="t('maintenanceHint')">
    <template v-if="devices.length > 1" #actions>
      <label class="maint__device-pick">
        <span>{{ t('maintenanceEpochDevice') }}</span>
        <select v-model="targetDeviceId" class="maint__select mono" :aria-label="t('maintenanceEpochDevice')">
          <option v-for="d in devices" :key="d.device" :value="d.device">
            {{ deviceName(d.device) }} ({{ d.device }})
          </option>
        </select>
      </label>
    </template>
    <div class="maint">
      <button type="button" class="btn btn--danger" :disabled="busy || requested" @click="confirming = true">
        <AppIcon name="power" :size="13" /> {{ busy ? t('restarting') : t('restartCore') }}
      </button>
      <button
        v-if="advancedMode"
        type="button"
        class="btn btn--danger"
        :disabled="busy || requested"
        @click="confirmingReset = true"
      >
        <AppIcon name="alert" :size="13" /> {{ t('factoryResetAction') }}
      </button>
      <span v-if="message" class="maint__msg" :class="`maint__msg--${tone}`">{{ message }}</span>
    </div>
    <p v-if="advancedMode" class="maint__reset-hint">{{ t('factoryResetHint') }}</p>

    <ConfirmDialog
      v-if="confirming"
      :title="t('restartConfirmTitle')"
      :message="t('restartConfirmBody')"
      :confirm-label="t('restartCore')"
      :cancel-label="t('cancel')"
      :busy="busy"
      :busy-label="t('restarting')"
      tone="danger"
      @confirm="restart"
      @cancel="confirming = false"
    />

    <ConfirmDialog
      v-if="confirmingReset"
      :title="t('factoryResetConfirmTitle')"
      :message="t('factoryResetConfirmBody')"
      :confirm-label="t('factoryResetConfirmLabel')"
      :cancel-label="t('cancel')"
      :busy="busy"
      :busy-label="t('saving')"
      tone="danger"
      @confirm="runFactoryReset"
      @cancel="confirmingReset = false"
    />
  </Panel>
</template>

<style scoped>
.maint {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.btn {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border: 1px solid var(--danger);
  background: transparent;
  color: var(--danger);
  border-radius: var(--r-sm);
  padding: 0.26rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:hover:not(:disabled) {
  background: var(--danger);
  color: var(--surface);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.maint__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.maint__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.maint__reset-hint {
  color: var(--muted);
  font-size: var(--fs-xs);
}

.maint__device-pick {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-xs);
  color: var(--muted);
}

.maint__select {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.16rem var(--sp-2);
  font-size: var(--fs-xs);
}
</style>
