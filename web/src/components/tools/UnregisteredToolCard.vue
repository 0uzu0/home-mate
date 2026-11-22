<script setup lang="ts">
/**
 * An honest placeholder for a tool backend that is not registered yet. It
 * exists so the Tools page shows the interface point without pretending there
 * is a configuration API behind it.
 */
import { t } from '../../i18n'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import EmptyState from '../EmptyState.vue'
import type { IconName } from '../icons'

withDefaults(
  defineProps<{
    title: string
    hint: string
    icon?: IconName
    stateLabel?: string
  }>(),
  { icon: 'unregistered', stateLabel: '' },
)
</script>

<template>
  <Panel :title="title" :hint="hint" :icon="icon" tone="warn">
    <template #actions>
      <Pill tone="warn" size="sm">{{ stateLabel || t('toolUnregistered') }}</Pill>
    </template>
    <EmptyState :title="t('toolUnregistered')" :hint="t('toolUnregisteredHint')" />
  </Panel>
</template>
