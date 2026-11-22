<script setup lang="ts">
import { computed, watch } from 'vue'
import { useRoute } from 'vue-router'
import { authToken } from '../auth'
import { t } from '../i18n'
import { usePoll } from '../hooks/usePoll'
import { confirmationIndexError, confirmationLoaded, confirmationLoading, confirmationCursor,
  sortedConfirmations, refreshConfirmation, refreshConfirmations } from '../stores/confirmations'
import ConfirmationCard from '../components/confirmations/ConfirmationCard.vue'
const route = useRoute()
const focused = computed(() => typeof route.query.action === 'string' ? route.query.action : null)
watch(focused, id => { if (id) void refreshConfirmation(id) }, { immediate: true })
const ids = computed(() => [...new Set([...(focused.value ? [focused.value] : []), ...sortedConfirmations.value.map(a => a.id)])])
usePoll(() => refreshConfirmations(), 3000)
</script>
<template>
  <section class="page">
    <header class="page__head"><div><h1 class="page__title">{{ t('confirmationsTitle') }}</h1><p class="page__sub">{{ t('confirmationsHint') }}</p></div></header>
    <p v-if="!authToken" role="alert">{{ t('confirmationsPairing') }}</p>
    <p v-if="confirmationIndexError" role="alert">{{ t('confirmationsStale') }}</p>
    <button type="button" class="btn" :disabled="!authToken || confirmationLoading" @click="refreshConfirmations()">{{ t('confirmationRefresh') }}</button>
    <p v-if="!confirmationLoaded && confirmationLoading" role="status">{{ t('confirmationsLoading') }}</p>
    <p v-if="confirmationLoaded && !ids.length">{{ t('confirmationsEmpty') }}</p>
    <ConfirmationCard v-for="id in ids" :id="id" :key="id" />
    <button v-if="confirmationCursor" type="button" class="btn" :disabled="confirmationLoading" @click="refreshConfirmations(true)">{{ t('confirmationsMore') }}</button>
  </section>
</template>
