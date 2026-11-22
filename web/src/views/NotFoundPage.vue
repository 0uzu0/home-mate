<script setup lang="ts">
/** Unknown console path: say so and offer the way back. */
import { RouterLink } from 'vue-router'
import { t } from '../i18n'
import EmptyState from '../components/EmptyState.vue'
</script>

<template>
  <section class="nf">
    <EmptyState :title="t('notFoundTitle')" :hint="t('notFoundHint')">
      <RouterLink class="nf__link" to="/status">{{ t('backHome') }}</RouterLink>
    </EmptyState>
  </section>
</template>

<style scoped>
.nf {
  padding: var(--sp-6) 0;
}

.nf__link {
  margin-top: var(--sp-2);
  font-size: var(--fs-sm);
}
</style>
