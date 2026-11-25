<script setup lang="ts">
/**
 * The physical ring. The colour is a hardware value from design/tokens.css
 * (`.led--<state>`) and intentionally does not follow the theme: a lamp is a
 * lamp. Unknown states render as idle rather than borrowing a colour.
 */
import { computed } from 'vue'
import { ledStateOf } from '../api'

const props = withDefaults(
  defineProps<{
    state: string | null | undefined
    size?: 'sm' | 'md' | 'lg' | 'xl'
    title?: string
  }>(),
  { size: 'md', title: '' },
)

const led = computed(() => ledStateOf(props.state))
const label = computed(() => props.state || 'idle')
</script>

<template>
  <span
    class="led"
    :class="[`led--${led}`, `led--${size}`]"
    :title="title || label"
    role="img"
    :aria-label="title || label"
  />
</template>

<style scoped>
.led {
  display: inline-block;
  flex: none;
  border-radius: 50%;
  border: 1px solid transparent;
  background: var(--led-idle);
}

.led--sm {
  width: 0.6rem;
  height: 0.6rem;
}

.led--md {
  width: 0.9rem;
  height: 0.9rem;
}

.led--lg {
  width: 1.6rem;
  height: 1.6rem;
}

.led--xl {
  width: 4rem;
  height: 4rem;
}

/* Idle is black: a hairline keeps it readable as a state, not a missing dot. */
.led--idle {
  border-color: var(--line-strong);
}

.led--listening {
  background: var(--led-listening);
}

.led--hearing {
  background: var(--led-hearing);
}

.led--wake {
  background: var(--led-wake);
}

.led--thinking {
  background: var(--led-thinking);
}

.led--speaking {
  background: var(--led-speaking);
}

.led--music {
  background: var(--led-music);
}

.led--paused {
  background: var(--led-paused);
}

.led--muted {
  background: var(--led-muted);
}

.led--error {
  background: var(--led-error);
}
</style>
