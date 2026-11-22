<script setup lang="ts">
/**
 * WiFi step: an online change. The device tries the new network and reverts
 * by itself within 20 s if it cannot join, so this cannot strand it.
 */
import { ref } from 'vue'
import { setDeviceWifi } from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import TextInput from '../TextInput.vue'

const props = defineProps<{ deviceId: string; online: boolean }>()
const emit = defineEmits<{ done: [] }>()

const ssid = ref('')
const psk = ref('')
const busy = ref(false)
const result = ref('')
const failed = ref(false)

async function apply(): Promise<void> {
  if (!props.deviceId) return
  busy.value = true
  result.value = ''
  failed.value = false
  try {
    const response = await setDeviceWifi(props.deviceId, ssid.value.trim(), psk.value)
    if (response.state === 'applied') {
      result.value = `${t('wifiApplied')} (${response.ssid})`
    } else if (response.state === 'reverted') {
      result.value = `${t('wifiReverted')} ${response.detail ?? ''}`
    } else {
      result.value = `${t('wifiPending')} ${response.detail ?? ''}`
    }
    failed.value = response.state !== 'applied'
    emit('done')
  } catch (err) {
    result.value = errorDetail(err)
    failed.value = true
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="wifi">
    <p v-if="!deviceId" class="wifi__hint">{{ t('setupDeviceMissing') }}</p>
    <div class="wifi__row">
      <TextInput v-model="ssid" :label="t('wifiSsid')" :placeholder="t('wifiSsidPlaceholder')" :disabled="busy || !online" />
      <TextInput v-model="psk" :label="t('wifiPsk')" type="password" :disabled="busy || !online" />
      <button
        type="button"
        class="btn"
        :disabled="busy || !online || !ssid.trim() || psk.length < 8"
        @click="apply"
      >
        {{ busy ? t('setupWorking') : t('wifiApply') }}
      </button>
    </div>
    <p class="wifi__hint">{{ t('wifiHint') }}</p>
    <p v-if="result" class="wifi__result" :class="{ 'wifi__result--bad': failed }">{{ result }}</p>
  </div>
</template>

<style scoped>
.wifi {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.wifi__row {
  display: flex;
  align-items: end;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.wifi__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.wifi__result {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.wifi__result--bad {
  color: var(--warn);
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.28rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
