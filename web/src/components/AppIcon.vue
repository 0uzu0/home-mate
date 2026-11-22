<script setup lang="ts">
/**
 * One icon, drawn from the shared symbol set. Decorative by default
 * (aria-hidden); pass `label` when the icon is the only content of a control.
 */
import { computed } from 'vue'
import { ICONS, type IconName } from './icons'

const props = withDefaults(
  defineProps<{
    name: IconName
    size?: number
    strokeWidth?: number
    label?: string
  }>(),
  { size: 16, strokeWidth: 1.8, label: '' },
)

const symbol = computed(() => ICONS[props.name])
</script>

<template>
  <component
    :is="symbol"
    class="icon"
    :size="size"
    :stroke-width="strokeWidth"
    :aria-hidden="label ? undefined : 'true'"
    :aria-label="label || undefined"
    :role="label ? 'img' : undefined"
  />
</template>

<style scoped>
.icon {
  flex: none;
  display: inline-block;
  vertical-align: -0.15em;
}
</style>
