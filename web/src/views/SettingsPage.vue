<script setup lang="ts">
/**
 * Settings page: grouped into 4 focused tabs so everyday settings stay fast
 * and scannable while assistant persona/memory, backup/maintenance, and
 * advanced fleet tools live in dedicated views.
 *
 * When Advanced Mode is toggled ON (from the header or General tab), the page
 * automatically switches to the Advanced & Fleet tab so ConfigPlanCard and
 * fleet controls are immediately visible.
 */
import { computed, nextTick, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { getStatus } from '../api'
import { mode, selected, refresh } from '../stores/devices'
import AgentPersonaCard from '../components/settings/AgentPersonaCard.vue'
import PromptCard from '../components/settings/PromptCard.vue'
import { t, type Key } from '../i18n'
import { useResource } from '../hooks/useResource'
import { usePoll } from '../hooks/usePoll'
import BackupCard from '../components/settings/BackupCard.vue'
import ConfigHistoryCard from '../components/service/ConfigHistoryCard.vue'
import LanguageCard from '../components/settings/LanguageCard.vue'
import WizardEntryCard from '../components/settings/WizardEntryCard.vue'
import AboutCard from '../components/settings/AboutCard.vue'
import MaintenanceCard from '../components/settings/MaintenanceCard.vue'
import MemoryCard from '../components/settings/MemoryCard.vue'
import WorkingMemoryCard from '../components/settings/WorkingMemoryCard.vue'
import AuthCard from '../components/settings/AuthCard.vue'
import FleetCard from '../components/settings/FleetCard.vue'
import Panel from '../components/Panel.vue'
import Toggle from '../components/Toggle.vue'
import { advancedMode, setAdvancedMode } from '../stores/advanced'
import ConfigPlanCard from '../components/config/ConfigPlanCard.vue'
import FleetRolloutCard from '../components/config/FleetRolloutCard.vue'

type SettingsTab = 'general' | 'assistant' | 'system' | 'advanced'

const route = useRoute()
const router = useRouter()

function parseTab(raw: unknown): SettingsTab {
  if (raw === 'assistant' || raw === 'system') return raw
  if (raw === 'advanced' && advancedMode.value) return 'advanced'
  return 'general'
}

const status = useResource((signal) => getStatus({ signal }), { timeoutMs: 5000, retries: 1 })
const resetKey = ref(0)
const activeTab = ref<SettingsTab>(parseTab(route?.query?.tab))
usePoll(() => (activeTab.value === 'general' ? status.load() : Promise.resolve()), 10000)

function selectTab(tab: SettingsTab): void {
  activeTab.value = tab
  if (router && route) {
    const query = { ...route.query }
    if (tab === 'general') delete query.tab
    else query.tab = tab
    void router.replace({ query }).catch(() => {})
  }
}

function onTabKeydown(event: KeyboardEvent, index: number): void {
  const list = tabs.value
  if (!list.length) return
  let nextIndex: number | null = null
  if (event.key === 'ArrowRight') nextIndex = (index + 1) % list.length
  else if (event.key === 'ArrowLeft') nextIndex = (index - 1 + list.length) % list.length
  else if (event.key === 'Home') nextIndex = 0
  else if (event.key === 'End') nextIndex = list.length - 1
  if (nextIndex === null) return
  event.preventDefault()
  const next = list[nextIndex]
  selectTab(next.id)
  void nextTick(() => {
    document.getElementById(`tab-${next.id}`)?.focus()
  })
}

watch(activeTab, (tab, prev) => {
  if (tab === 'general' && prev !== 'general') void status.load()
})

const tabs = computed<{ id: SettingsTab; labelKey: Key }[]>(() => {
  const list: { id: SettingsTab; labelKey: Key }[] = [
    { id: 'general', labelKey: 'settingsTabGeneral' },
    { id: 'assistant', labelKey: 'settingsTabAssistant' },
    { id: 'system', labelKey: 'settingsTabSystem' },
  ]
  if (advancedMode.value) {
    list.push({ id: 'advanced', labelKey: 'settingsTabAdvanced' })
  }
  return list
})

watch(
  () => route?.query?.tab,
  (q) => {
    activeTab.value = parseTab(q)
  },
)

watch(advancedMode, (enabled) => {
  if (enabled) {
    selectTab('advanced')
  } else if (activeTab.value === 'advanced') {
    selectTab('general')
  }
})

function onFactoryReset(): void {
  resetKey.value += 1
  void status.load()
}
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('settingsTitle') }}</h1>
        <p class="page__sub">{{ t('settingsSub') }}</p>
      </div>
      <nav class="settings-tabs" role="tablist" :aria-label="t('settingsTitle')">
        <button
          v-for="(tab, index) in tabs"
          :id="`tab-${tab.id}`"
          :key="tab.id"
          type="button"
          role="tab"
          class="settings-tabs__btn"
          :class="{ 'settings-tabs__btn--active': activeTab === tab.id }"
          :aria-selected="activeTab === tab.id"
          :aria-controls="`panel-${tab.id}`"
          :tabindex="activeTab === tab.id ? 0 : -1"
          @click="selectTab(tab.id)"
          @keydown="onTabKeydown($event, index)"
        >
          {{ t(tab.labelKey) }}
        </button>
      </nav>
    </header>

    <div
      v-if="activeTab === 'general'"
      id="panel-general"
      class="settings-grid settings-grid--two"
      role="tabpanel"
      aria-labelledby="tab-general"
    >
      <LanguageCard />
      <AuthCard :key="`auth-${resetKey}`" />
      <Panel icon="settings" :title="t('advancedTitle')" :hint="t('advancedHint')">
        <Toggle
          :model-value="advancedMode"
          :label="t('advancedToggle')"
          @update:model-value="setAdvancedMode"
        />
      </Panel>
      <WizardEntryCard />
      <div class="settings-grid__full">
        <AboutCard :status="status.data.value" />
      </div>
    </div>

    <div
      v-else-if="activeTab === 'assistant'"
      id="panel-assistant"
      class="settings-grid"
      role="tabpanel"
      aria-labelledby="tab-assistant"
    >
      <PromptCard :key="`prompt-${resetKey}`" :mode="mode" :device="selected" :refresh="refresh" />
      <MemoryCard :key="`mem-${resetKey}`" />
      <AgentPersonaCard :key="`persona-${resetKey}`" />
      <WorkingMemoryCard v-if="advancedMode" :key="`wm-${resetKey}`" />
    </div>

    <div
      v-else-if="activeTab === 'system'"
      id="panel-system"
      class="settings-grid"
      role="tabpanel"
      aria-labelledby="tab-system"
    >
      <BackupCard :key="`backup-${resetKey}`" />
      <MaintenanceCard @reset="onFactoryReset" />
    </div>

    <div
      v-else-if="activeTab === 'advanced' && advancedMode"
      id="panel-advanced"
      class="settings-grid"
      role="tabpanel"
      aria-labelledby="tab-advanced"
    >
      <ConfigPlanCard :key="`plan-${resetKey}`" />
      <div :key="`fleet-${resetKey}`" id="fleet-identity"><FleetCard /></div>
      <FleetRolloutCard :key="`fleet-v2-${resetKey}`" />
      <ConfigHistoryCard :key="`hist-${resetKey}`" />
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
  flex-direction: column;
  gap: var(--sp-3);
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

.settings-tabs {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  padding: var(--sp-1);
  border: 1px solid var(--line);
  border-radius: var(--r-md);
  background: var(--surface-2);
  width: fit-content;
  max-width: 100%;
}

.settings-tabs__btn {
  padding: 0.45rem 0.95rem;
  border: 1px solid transparent;
  border-radius: var(--r-sm);
  background: transparent;
  color: var(--muted);
  font-size: var(--fs-sm);
  font-weight: 500;
  cursor: pointer;
  transition: background var(--dur-1) var(--ease), color var(--dur-1) var(--ease), border-color var(--dur-1) var(--ease);
}

.settings-tabs__btn:hover {
  color: var(--ink);
}

.settings-tabs__btn--active {
  background: var(--surface);
  color: var(--ink);
  border-color: var(--line);
  box-shadow: var(--shadow-card);
}

.settings-grid {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.settings-grid--two {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--sp-4);
  align-items: start;
}

.settings-grid__full {
  grid-column: 1 / -1;
}

@media (max-width: 640px) {
  .settings-tabs {
    width: 100%;
  }

  .settings-tabs__btn {
    flex: 1 1 auto;
    text-align: center;
  }

  .settings-grid--two {
    grid-template-columns: 1fr;
  }
}
</style>
