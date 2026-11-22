<script setup lang="ts">
/**
 * Services page: EXTERNAL services only — the conversation model, its
 * fallback, speech recognition and speech synthesis. Tools (music, personal
 * services, weather, news, Home Assistant) live on the Tools page.
 */
import { computed } from 'vue'
import { getServices } from '../api'
import { t } from '../i18n'
import { useResource } from '../hooks/useResource'
import { usePoll } from '../hooks/usePoll'
import LoadingBlock from '../components/LoadingBlock.vue'
import ErrorNotice from '../components/ErrorNotice.vue'
import ProviderCard from '../components/service/ProviderCard.vue'

const services = useResource((signal) => getServices({ signal }), { timeoutMs: 6000, retries: 1 })
const refresh = () => services.load()
usePoll(refresh, 5000)

const providers = computed(() => [
  { service: 'llm' as const, title: t('serviceLlm') },
  { service: 'llm_fallback' as const, title: t('serviceLlmFallback') },
  { service: 'asr' as const, title: t('serviceAsr') },
  { service: 'tts' as const, title: t('serviceTts') },
])
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('servicesTitle') }}</h1>
        <p class="page__sub">{{ t('servicesSub') }}</p>
      </div>
      <button type="button" class="btn" :disabled="services.loading.value" @click="refresh">
        {{ t('refresh') }}
      </button>
    </header>

    <ErrorNotice
      v-if="services.error.value"
      :error="services.error.value"
      :compact="services.data.value !== null"
      @retry="refresh"
    />
    <LoadingBlock v-else-if="services.loading.value && !services.data.value" :label="t('loading')" />

    <div class="page__grid">
      <ProviderCard
        v-for="provider in providers"
        :key="provider.service"
        :service="provider.service"
        :title="provider.title"
        :status="services.data.value?.[provider.service] ?? null"
        :refresh="refresh"
      />
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

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.22rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
