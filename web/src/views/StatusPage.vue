<script setup lang="ts">
/**
 * Status page: only orchestration. Three cards own the three objects — the
 * board (read-only facts), the program (pause/continue) and the outputs
 * (stop). The snapshot and the selected device come from the app-wide store;
 * the shell owns the single poll.
 */
import { computed } from 'vue'
import { t } from '../i18n'
import {
  gateway,
  loading,
  mode,
  refresh,
  selected,
  selectedGateway,
  wifiProvisioning,
} from '../stores/devices'
import LoadingBlock from '../components/LoadingBlock.vue'
import DeviceFreshnessNotice from '../components/DeviceFreshnessNotice.vue'
import EmptyState from '../components/EmptyState.vue'
import AppIcon from '../components/AppIcon.vue'
import ReadinessCard from '../components/status/ReadinessCard.vue'
import StatusBoard from '../components/status/StatusBoard.vue'
import ProgramCard from '../components/status/ProgramCard.vue'
import OutputsCard from '../components/status/OutputsCard.vue'

const loadingFirst = computed(() => !mode.value && loading.value)
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('statusTitle') }}</h1>
        <p class="page__sub">{{ t('statusSub') }}</p>
      </div>
      <div class="page__tools">
        <button type="button" class="btn" :disabled="loading" @click="refresh">
          <AppIcon name="reload" :size="14" /> {{ t('refresh') }}
        </button>
      </div>
    </header>

    <div v-if="wifiProvisioning.requested" class="wifi-banner" role="status">
      <div class="wifi-banner__body">
        <strong>{{ t('wifiProvisioningBannerTitle') }}</strong>
        <p>{{ t('wifiProvisioningBannerHint', { device: wifiProvisioning.device || 'dot2' }) }}</p>
      </div>
      <RouterLink class="btn" to="/setup">
        <AppIcon name="wizard" :size="14" /> {{ t('wifiProvisioningGoSetup') }}
      </RouterLink>
    </div>

    <DeviceFreshnessNotice />
    <LoadingBlock v-if="loadingFirst" :label="t('loading')" />
    <template v-else>
      <!-- Readiness first: on a fresh install this is the only thing that
           tells the user what is still missing (audit 2026-11-14). -->
      <ReadinessCard :mode="mode" :device="selected" />
      <EmptyState v-if="!mode?.devices.length" :title="t('statusNoDevices')" :hint="t('firstRunHint')">
        <div class="first-run">
          <p class="first-run__title">{{ t('firstRunTitle') }}</p>
          <ol class="first-run__steps">
            <li><RouterLink to="/services">{{ t('firstRunServices') }}</RouterLink></li>
            <li><RouterLink to="/devices">{{ t('firstRunDevice') }}</RouterLink></li>
            <li><RouterLink to="/interaction">{{ t('firstRunWake') }}</RouterLink></li>
          </ol>
          <RouterLink class="btn" to="/setup">
            <AppIcon name="wizard" :size="14" /> {{ t('firstRunWizard') }}
          </RouterLink>
        </div>
      </EmptyState>
    </template>

    <div v-if="mode?.devices.length" class="page__grid">
      <StatusBoard :device="selected" :gateway="gateway" :gateway-device="selectedGateway" />
      <ProgramCard :device="selected" :refresh="refresh" />
      <OutputsCard :device="selected" :gateway="gateway" :refresh="refresh" />
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

.page__tools {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.page__grid {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.wifi-banner {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-3);
  flex-wrap: wrap;
  border: 1px solid var(--warn);
  background: var(--surface-2);
  border-radius: var(--r-md);
  padding: var(--sp-3) var(--sp-4);
  font-size: var(--fs-sm);
}

.wifi-banner__body p {
  margin-top: var(--sp-1);
  color: var(--muted);
}

.first-run {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  margin-top: var(--sp-2);
}

.first-run__title {
  font-size: var(--fs-sm);
  font-weight: 650;
  color: var(--ink-2);
}

.first-run__steps {
  margin: 0;
  padding-left: 1.15rem;
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  font-size: var(--fs-sm);
}

.first-run__steps a {
  color: var(--accent);
}
</style>
