<script setup lang="ts">
/** Version facts: what is running, on which device, from which console build. */
import { computed } from 'vue'
import type { Status } from '../../api'
import { t } from '../../i18n'
import Panel from '../Panel.vue'
import BrandLogo from '../BrandLogo.vue'
import StatTile from '../StatTile.vue'

const props = defineProps<{ status: Status | null }>()

const devices = computed(() => props.status?.gateway.devices ?? [])
const deviceSummary = computed(() => {
  if (!devices.value.length) return '—'
  return devices.value.map((d) => d.stable_id).join(' · ')
})
const firmwareSummary = computed(() => {
  const fws = devices.value
    .map((d) => (d.firmware ? `${d.stable_id}: ${d.firmware}` : ''))
    .filter(Boolean)
  return fws.join(' / ')
})

const consoleBuild = computed(() => {
  const script = Array.from(document.scripts)
    .map((item) => item.src)
    .find((src) => src.includes('/assets/'))
  if (!script) return t('aboutBuildUnknown')
  return script.split('/').pop() ?? t('aboutBuildUnknown')
})
</script>

<template>
  <Panel
    icon="about" :title="t('aboutTitle')">
    <div class="about__brand">
      <BrandLogo class="about__logo" />
      <span class="about__tag">{{ t('brandSub') }}</span>
    </div>
    <div class="about__grid">
      <StatTile :label="t('aboutProduct')" :value="status?.product || t('brand')" tone="muted" />
      <StatTile :label="t('aboutVersion')" :value="status?.version ?? '—'" tone="accent" />
      <StatTile :label="t('aboutDevice')" :value="deviceSummary" tone="muted" :note="firmwareSummary" />
      <StatTile :label="t('aboutConsoleBuild')" :value="consoleBuild" tone="muted" />
    </div>
  </Panel>
</template>

<style scoped>
.about__brand {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.about__logo {
  height: 3rem;
  width: auto;
  border-radius: var(--r-sm);
}

.about__tag {
  font-size: var(--fs-sm);
  color: var(--muted);
}

.about__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(10rem, 1fr));
  gap: var(--sp-2);
}
</style>
