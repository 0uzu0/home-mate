<script setup lang="ts">
/** Console language. The choice is per browser and applies without a reload. */
import { locale, setLocale, t, type Locale } from '../../i18n'
import Panel from '../Panel.vue'

function choose(next: Locale): void {
  setLocale(next)
}

const choices: Array<{ value: Locale; label: string }> = [
  { value: 'zh', label: t('languageZh') },
  { value: 'en', label: t('languageEn') },
]
</script>

<template>
  <Panel
    icon="language" :title="t('languageTitle')" :hint="t('languageHint')">
    <div class="lang" role="group" :aria-label="t('appLanguage')">
      <button
        v-for="choice in choices"
        :key="choice.value"
        type="button"
        class="lang__choice"
        :class="{ 'lang__choice--active': locale === choice.value }"
        :aria-pressed="locale === choice.value"
        @click="choose(choice.value)"
      >
        {{ choice.label }}
      </button>
    </div>
  </Panel>
</template>

<style scoped>
.lang {
  display: flex;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.lang__choice {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink-2);
  border-radius: var(--r-pill);
  padding: 0.3rem var(--sp-4);
  cursor: pointer;
}

.lang__choice--active {
  background: var(--accent-soft);
  border-color: var(--accent-line);
  color: var(--accent);
  font-weight: 600;
}
</style>
