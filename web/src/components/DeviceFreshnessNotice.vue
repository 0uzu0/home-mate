<script setup lang="ts">
import { computed } from 'vue'
import { t } from '../i18n'
import {
  mode, gateway, metaLoaded, modeError, gatewayError, metaError,
  modeUpdatedAt, gatewayUpdatedAt, metaUpdatedAt, refresh,
} from '../stores/devices'
import ErrorNotice from './ErrorNotice.vue'

const resources = computed(() => [
  { key: 'mode', label: t('modeSnapshotLabel'), error: modeError.value, stale: mode.value !== null, at: modeUpdatedAt.value },
  { key: 'gateway', label: t('gatewaySnapshotLabel'), error: gatewayError.value, stale: gateway.value !== null, at: gatewayUpdatedAt.value },
  { key: 'meta', label: t('metadataSnapshotLabel'), error: metaError.value, stale: metaLoaded.value, at: metaUpdatedAt.value },
].filter(resource => resource.error))
</script>

<template>
  <section v-for="resource in resources" :key="resource.key" :aria-label="resource.label">
    <p class="resource-label">{{ resource.label }}</p>
    <ErrorNotice :error="resource.error" :stale="resource.stale" :at="resource.at" compact @retry="refresh" />
  </section>
</template>

<style scoped>
.resource-label { color: var(--muted); font-size: var(--fs-sm); margin-bottom: var(--sp-1); }
</style>
