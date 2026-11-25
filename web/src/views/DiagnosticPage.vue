<script setup lang="ts">
/**
 * Diagnostics page: passive snapshot, active probes/repairs, traces and text
 * chat. The snapshot polls; probes and chat only act when asked.
 */
import { computed, ref } from 'vue'
import { getDiagnostics } from '../api'
import { t } from '../i18n'
import { useResource } from '../hooks/useResource'
import { usePoll } from '../hooks/usePoll'
import { devices, selected } from '../stores/devices'
import SnapshotCard from '../components/diagnostics/SnapshotCard.vue'
import SelfCheckCard from '../components/diagnostics/SelfCheckCard.vue'
import ProbesCard from '../components/diagnostics/ProbesCard.vue'
import TracesCard from '../components/diagnostics/TracesCard.vue'
import ChatCard from '../components/diagnostics/ChatCard.vue'
import WakeLogCard from '../components/diagnostics/WakeLogCard.vue'

const snapshot = useResource((signal) => getDiagnostics({ signal }), { timeoutMs: 6000, retries: 1 })
const refresh = () => snapshot.load()
usePoll(refresh, 5000)

const focusTrace = ref<string | null>(null)

// Prefer the live store; the snapshot's copy is the fallback when the shell
// poll has not answered yet.
const probeDevices = computed(() => (devices.value.length ? devices.value : snapshot.data.value?.devices ?? []))
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('diagTitle') }}</h1>
        <p class="page__sub">{{ t('diagSub') }}</p>
      </div>
    </header>

    <div class="page__grid">
      <SnapshotCard
        :data="snapshot.data.value"
        :error="snapshot.error.value"
        :loading="snapshot.loading.value"
        :refresh="refresh"
      />
      <SelfCheckCard :devices="probeDevices" :selected-device-id="selected?.device ?? null" />
      <ProbesCard :devices="probeDevices" :selected-device-id="selected?.device ?? null" />
      <WakeLogCard />
      <TracesCard :focus="focusTrace" @clear-focus="focusTrace = null" />
      <ChatCard @open-trace="focusTrace = $event" />
    </div>
  </section>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
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
</style>
