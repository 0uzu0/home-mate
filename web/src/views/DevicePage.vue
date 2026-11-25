<script setup lang="ts">
/**
 * Devices page: the device list (all reported devices), then the selected
 * device's volume, light and microphone, plus the household-wide audio gates
 * and firmware. This is RUI's list + detail split: the list scales to more
 * devices, the detail never assumes `[0]`.
 */
import { computed } from 'vue'
import { t } from '../i18n'
import { advancedMode } from '../stores/advanced'
import {
  deviceName,
  deviceRoom,
  devices,
  gatewayDevices,
  loading,
  meta,
  metaError,
  metaLoaded,
  mode,
  refresh,
  rooms,
  selectDevice,
  selected,
  selectedGateway,
} from '../stores/devices'
import LoadingBlock from '../components/LoadingBlock.vue'
import DeviceFreshnessNotice from '../components/DeviceFreshnessNotice.vue'
import EmptyState from '../components/EmptyState.vue'
import AppIcon from '../components/AppIcon.vue'
import DeviceListCard from '../components/device/DeviceListCard.vue'
import DeviceMetaCard from '../components/device/DeviceMetaCard.vue'
import AudioGateCard from '../components/device/AudioGateCard.vue'
import VolumeCard from '../components/device/VolumeCard.vue'
import ToneCard from '../components/device/ToneCard.vue'
import LedCard from '../components/device/LedCard.vue'
import MuteCard from '../components/device/MuteCard.vue'
import MicCard from '../components/device/MicCard.vue'
import AcousticsCard from '../components/device/AcousticsCard.vue'
import ReminderCard from '../components/device/ReminderCard.vue'
import FirmwareCard from '../components/device/FirmwareCard.vue'

const loadingFirst = computed(() => !mode.value && loading.value)
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('devicesTitle') }}</h1>
        <p class="page__sub">{{ t('devicesSub') }}</p>
      </div>
      <button type="button" class="btn" :disabled="loading" @click="refresh">
        <AppIcon name="reload" :size="14" /> {{ t('refresh') }}
      </button>
    </header>

    <DeviceFreshnessNotice />
    <LoadingBlock v-if="loadingFirst" :label="t('loading')" />
    <EmptyState v-else-if="!devices.length" :title="t('noDevice')" />

    <div v-else class="page__grid">
      <DeviceListCard
        :devices="devices"
        :gateway-devices="gatewayDevices"
        :meta="meta"
        :selected-id="selected?.device ?? null"
        @select="selectDevice"
      />

      <template v-if="selected">
        <DeviceMetaCard
          :key="`meta-${selected.device}`"
          :device="selected"
          :meta="meta[selected.device]"
          :meta-loaded="metaLoaded"
          :meta-error="metaError"
          :rooms="rooms"
          :refresh="refresh"
        />
        <AudioGateCard v-if="advancedMode" :key="`gate-${selected.device}`" :mode="mode" :device="selected" :refresh="refresh" />

        <!-- Audit R2-2: the tone curve is one household-wide setting, so it
             must not sit inside the selected device's block where it reads as
             "this speaker's". -->
        <section class="group">
          <h2 class="group__title">{{ t('scopeSharedTitle') }}</h2>
          <p class="group__sub">{{ t('scopeSharedSub') }}</p>
          <ToneCard />
        </section>

        <section :key="selected.device" class="device">
          <header class="device__head">
            <h2 class="device__name">{{ deviceName(selected.device) }}</h2>
            <span class="device__meta mono">
              {{ selected.device }}
              <template v-if="deviceRoom(selected.device)"> · {{ deviceRoom(selected.device) }}</template>
              · {{ selectedGateway?.model || t('unknown') }}
              <template v-if="selectedGateway?.firmware">
                · {{ t('firmware') }}: {{ selectedGateway.firmware }}
              </template>
            </span>
          </header>
          <VolumeCard
            :device="selected"
            :gateway-device-id="selectedGateway?.stable_id ?? null"
            :volume="selectedGateway?.volume ?? null"
          />
          <!-- protocol v1 (#8/#13): the remote mute latch — second writer
               beside the physical key, with its own provenance badge. -->
          <MuteCard :device="selected" :gateway-device-id="selectedGateway?.stable_id ?? null" />
          <LedCard :device="selected" :gateway-device="selectedGateway" />
          <ReminderCard :key="`rem-${selected.device}`" :device="selected" :devices="devices" />
          <MicCard v-if="advancedMode" :device="selected" :gateway-device="selectedGateway" />
          <AcousticsCard v-if="advancedMode" :key="`ac-${selected.device}`" :device="selected" />
        </section>
      </template>

      <FirmwareCard v-if="advancedMode" />
    </div>
  </section>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.page__head {
  display: flex;
  align-items: flex-end;
  justify-content: space-between;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.page__title {
  font-size: var(--fs-xl);
}

.page__sub {
  margin-top: var(--sp-1);
  color: var(--muted);
  font-size: var(--fs-sm);
  max-width: 75ch;
}

.page__grid {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.device {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.group {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.group__title {
  font-size: var(--fs-md);
  font-weight: 650;
}

.group__sub {
  color: var(--muted);
  font-size: var(--fs-sm);
  max-width: 75ch;
}

.device__head {
  display: flex;
  align-items: baseline;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.device__name {
  font-size: var(--fs-lg);
}

.device__meta {
  font-size: var(--fs-xs);
  color: var(--muted);
}
</style>
