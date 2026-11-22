<script setup lang="ts">
import { RouterLink } from 'vue-router'
import { t } from '../../i18n'
import { usePoll } from '../../hooks/usePoll'
import { confirmationIndexError, pendingConfirmations, refreshConfirmations } from '../../stores/confirmations'
usePoll(() => refreshConfirmations(), 3000)
</script>
<template>
  <aside v-if="pendingConfirmations.length || confirmationIndexError" class="confirmation-notice" role="status">
    <span>{{ confirmationIndexError ? t('confirmationsStale') : t('confirmationsBanner', { count: pendingConfirmations.length }) }}</span>
    <RouterLink class="btn" to="/confirmations">{{ t('confirmationsOpen') }}</RouterLink>
  </aside>
</template>
<style scoped>
.confirmation-notice { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: var(--sp-3); padding: var(--sp-3); border: 1px solid var(--warn-line); background: var(--warn-soft); border-radius: var(--r-md); }
</style>
