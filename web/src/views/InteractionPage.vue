<script setup lang="ts">
/**
 * Interaction page (#62): 4 cohesive intent-based panels:
 *   1) Wake & Response (唤醒与响应)
 *   2) Continuous Conversation / Follow-up Window (连续对话（应答窗）)
 *   3) Interrupt & Reply Style (打断与播报风格)
 *   4) Multi-Speaker Coordination (多音箱协同)
 *
 * Expert-only diagnostics & tuning (WakeMethodsCard, DuplexCard, BargeCard,
 * raw threshold/score, token compiler) are gated strictly behind advancedMode.
 */
import { computed, ref } from 'vue'
import { t } from '../i18n'
import { advancedMode } from '../stores/advanced'
import {
  loading,
  mode,
  refresh,
  selected,
} from '../stores/devices'
import AppIcon from '../components/AppIcon.vue'
import Pill from '../components/Pill.vue'
import LoadingBlock from '../components/LoadingBlock.vue'
import DeviceFreshnessNotice from '../components/DeviceFreshnessNotice.vue'
import EmptyState from '../components/EmptyState.vue'
import WakeResponsePanel from '../components/interaction/WakeResponsePanel.vue'
import FollowUpPanel from '../components/interaction/FollowUpPanel.vue'
import InterruptStylePanel from '../components/interaction/InterruptStylePanel.vue'
import ArbitrationCard from '../components/interaction/ArbitrationCard.vue'
import WakeMethodsCard from '../components/interaction/WakeMethodsCard.vue'
import DuplexCard from '../components/interaction/DuplexCard.vue'
import BargeCard from '../components/interaction/BargeCard.vue'

const loadingFirst = computed(() => !mode.value && loading.value)
const refreshKey = ref(0)

async function refreshAll(): Promise<void> {
  await refresh()
  refreshKey.value += 1
}
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('interactionTitle') }}</h1>
        <p class="page__sub">{{ t('interactionSub') }}</p>
      </div>
      <div class="page__actions">
        <Pill v-if="selected" size="sm" tone="accent" mono>
          {{ t('scopeThisDevice', { device: selected.device }) }}
        </Pill>
        <button type="button" class="btn" :disabled="loading" @click="refreshAll">
          <AppIcon name="reload" :size="14" /> {{ loading ? t('loading') : t('refresh') }}
        </button>
      </div>
    </header>

    <DeviceFreshnessNotice />
    <LoadingBlock v-if="loadingFirst" :label="t('loading')" />
    <EmptyState v-else-if="!selected" :title="t('noDevice')" />

    <div v-else :key="`${selected.device}-${refreshKey}`" class="page__grid">
      <div id="wake-words" class="anchor">
        <WakeResponsePanel :mode="mode" :device="selected" :refresh="refresh" />
      </div>

      <div id="follow-window" class="anchor">
        <FollowUpPanel :mode="mode" :device="selected" :refresh="refresh" />
      </div>

      <div id="command-words" class="anchor">
        <InterruptStylePanel :mode="mode" :device="selected" :refresh="refresh" />
      </div>

      <div id="wake-arbitration" class="anchor">
        <ArbitrationCard :mode="mode" :device="selected" :refresh="refresh" />
      </div>

      <details v-if="advancedMode" class="group__advanced">
        <summary>{{ t('sectionAdvancedTitle') }}</summary>
        <div class="group__advanced-cards">
          <WakeMethodsCard :mode="mode" :device="selected" />
          <DuplexCard :mode="mode" :refresh="refresh" />
          <BargeCard
            v-if="selected.duplex.effective === 'full'"
            :mode="mode"
            :device="selected"
            :refresh="refresh"
          />
        </div>
      </details>
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

.page__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
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

.group__advanced {
  border: 1px solid var(--line);
  border-radius: var(--r-md, var(--r-sm));
  padding: var(--sp-3);
  background: var(--surface);
}

.group__advanced > summary {
  cursor: pointer;
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--muted);
}

.group__advanced-cards {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  margin-top: var(--sp-3);
}

.anchor {
  scroll-margin-top: var(--sp-4);
}
</style>
