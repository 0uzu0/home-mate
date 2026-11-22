<script setup lang="ts">
/**
 * Remote mute (protocol v1 #8/#13, 2026-11-16).
 *
 * The latch has exactly TWO writers — the physical key and this explicit
 * frame — and the card shows WHO last wrote it. The answer below the buttons
 * is the DEVICE's receipt (or an honest unknown), never the request: a send
 * is not an application. A firmware without the `remote_mute` capability
 * answers 409 and the card says so instead of pretending.
 */
import { computed } from 'vue'
import { setDeviceMute, type ModeDevice } from '../../api'
import { t } from '../../i18n'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import AppIcon from '../AppIcon.vue'
import LedDot from '../LedDot.vue'

const props = defineProps<{
  device: ModeDevice | null
  /** stable_id of the device on the gateway link (null = not announced). */
  gatewayDeviceId: string | null
}>()

const { busy, message, tone, run } = useSave()

const muted = computed(() => props.device?.muted)
const latchLabel = computed(() => {
  if (muted.value === true) return t('muteOn')
  if (muted.value === false) return t('muteOff')
  return t('muteUnknown')
})

/** The source badge: who last wrote the latch (a device fact, not a guess). */
const sourceLabel = computed(() => {
  const src = props.device?.mute_source
  if (!src) return ''
  if (src === 'core_set') return t('muteSourceCore')
  if (src === 'button' || src === 'hardware') return t('muteSourceKey')
  if (src === 'device_boot') return t('muteSourceBoot')
  return src
})

const canAct = computed(() => !!props.gatewayDeviceId)

function act(muted: boolean) {
  const id = props.gatewayDeviceId
  if (!id) return
  void run(async () => {
    const r = await setDeviceMute(id, muted)
    if (r.applied) {
      return t('muteApplied', {
        state: r.muted ? t('muteOn') : t('muteOff'),
        source: r.source ?? '',
      })
    }
    // Honest unknown: the command went out, the device did not answer.
    return t('muteUnknownResult')
  })
}
</script>

<template>
  <Panel icon="micOff" apply="immediate" :title="t('muteCardTitle')" :hint="t('muteCardHint')">
    <template #actions>
      <Pill :tone="muted === true ? 'danger' : muted === false ? 'ok' : 'neutral'" size="sm">
        <LedDot :state="muted === true ? 'muted' : muted === false ? 'listening' : 'idle'" size="sm" />
        {{ latchLabel }}
        <span v-if="sourceLabel" class="mute-source mono">· {{ sourceLabel }}</span>
      </Pill>
    </template>

    <div class="mute">
      <p class="mute__rule">{{ t('muteCardRule') }}</p>
      <div class="mute__buttons">
        <button
          type="button"
          class="btn btn--danger"
          :disabled="busy || !canAct || muted === true"
          @click="act(true)"
        >
          <AppIcon name="micOff" :size="14" /> {{ t('muteActSet') }}
        </button>
        <button
          type="button"
          class="btn"
          :disabled="busy || !canAct || muted === false"
          @click="act(false)"
        >
          <AppIcon name="mic" :size="14" /> {{ t('muteActClear') }}
        </button>
      </div>
      <p v-if="message" class="mute__result" :class="{ 'mute__result--error': tone === 'error' }">
        {{ message }}
      </p>
    </div>
  </Panel>
</template>

<style scoped>
.mute {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.mute__rule {
  color: var(--muted);
  font-size: var(--fs-sm);
  max-width: 75ch;
}

.mute__buttons {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.btn {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.3rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.btn--danger {
  border-color: var(--danger, var(--led-muted, #780000));
}

.mute__result {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.mute__result--error {
  color: var(--danger, var(--led-error, #aa0000));
}

.mute-source {
  color: var(--muted);
  font-size: var(--fs-xs);
}
</style>
