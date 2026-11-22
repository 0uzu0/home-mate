<script setup lang="ts">
/** Entry to the setup wizard. The wizard itself is a route, not a modal. */
import { RouterLink } from 'vue-router'
import { t } from '../../i18n'
import Panel from '../Panel.vue'

defineEmits<{ open: [] }>()
</script>

<template>
  <Panel
    icon="wizard" :title="t('wizardEntryTitle')" :hint="t('wizardEntryHint')">
    <div>
      <RouterLink class="wizard__link" to="/setup">{{ t('wizardOpen') }}</RouterLink>
    </div>
  </Panel>
</template>

<style scoped>
.wizard__link {
  display: inline-block;
  border: 1px solid var(--accent);
  background: var(--accent);
  color: var(--on-accent);
  border-radius: var(--r-sm);
  padding: 0.3rem var(--sp-4);
  font-size: var(--fs-sm);
}

.wizard__link:hover {
  background: var(--accent-hover);
  text-decoration: none;
}
</style>
