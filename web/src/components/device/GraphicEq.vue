<script setup lang="ts">
/**
 * A ten-band graphic EQ: one vertical fader per octave centre, the gain in dB
 * above it and the frequency below — the shape people know from every music
 * player, and the one the user asked for instead of a curve.
 *
 * Values are 0.5 dB steps within ±12 dB (the device accepts ±18; the sliders
 * stay inside a range this small driver can use). Dragging, the wheel and the
 * arrow keys all move the same value; each fader is a real ARIA slider.
 */
import { computed, ref } from 'vue'

const props = defineProps<{
  modelValue: number[]
  bands: number[]
  disabled?: boolean
}>()

const emit = defineEmits<{ 'update:modelValue': [number[]] }>()

const MIN = -12
const MAX = 12
const STEP = 0.5
/** Pixels of travel for the full 24 dB range, and its inset from the track
 * ends so the thumb never hangs outside. */
const TRAVEL = 150
const PAD = 14

const dragging = ref<number | null>(null)

const values = computed(() => props.modelValue)

const FREQ_LABEL: Record<number, string> = {
  31.5: '31.5',
  63: '63',
  125: '125',
  250: '250',
  500: '500',
  1000: '1K',
  2000: '2K',
  4000: '4K',
  8000: '8K',
  16000: '16K',
}

function clampStep(v: number): number {
  return Math.max(MIN, Math.min(MAX, Math.round(v / STEP) * STEP))
}

function setAt(i: number, v: number): void {
  if (props.disabled) return
  const next = props.modelValue.slice()
  next[i] = clampStep(v)
  emit('update:modelValue', next)
}

function format(v: number): string {
  return (v > 0 ? '+' : '') + v.toFixed(1)
}

/** Thumb centre from the top of the track: +12 dB at the top of the travel. */
function thumbY(v: number): number {
  return PAD + TRAVEL * (1 - (v - MIN) / (MAX - MIN))
}

function onPointerDown(e: PointerEvent, i: number): void {
  if (props.disabled) return
  dragging.value = i
  ;(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId)
  move(e, i)
}

function onPointerMove(e: PointerEvent, i: number): void {
  if (dragging.value !== i) return
  move(e, i)
}

function onPointerUp(e: PointerEvent, i: number): void {
  if (dragging.value === i) dragging.value = null
  ;(e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId)
}

function move(e: PointerEvent, i: number): void {
  const el = e.currentTarget as HTMLElement
  const r = el.getBoundingClientRect()
  // Track is TRAVEL px tall; the top is +12 dB.
  const rel = (e.clientY - (r.top + PAD)) / TRAVEL
  const v = MAX - rel * (MAX - MIN)
  setAt(i, v)
}

function onWheel(e: WheelEvent, i: number): void {
  if (props.disabled) return
  e.preventDefault()
  setAt(i, props.modelValue[i] + (e.deltaY < 0 ? STEP : -STEP))
}

function onKey(e: KeyboardEvent, i: number): void {
  const cur = props.modelValue[i]
  const map: Record<string, number> = {
    ArrowUp: cur + STEP,
    ArrowRight: cur + STEP,
    ArrowDown: cur - STEP,
    ArrowLeft: cur - STEP,
    PageUp: cur + 3,
    PageDown: cur - 3,
    Home: MAX,
    End: MIN,
  }
  if (e.key in map) {
    e.preventDefault()
    setAt(i, map[e.key])
  } else if (e.key === '0' || e.key === 'Backspace' || e.key === 'Delete') {
    e.preventDefault()
    setAt(i, 0)
  }
}
</script>

<template>
  <div class="geq" :class="{ 'geq--disabled': disabled }">
    <div class="geq__banner" aria-hidden="true">
      <span class="geq__db geq__db--top">+12 dB</span>
      <span class="geq__db geq__db--mid">0 dB</span>
      <span class="geq__db geq__db--bottom">−12 dB</span>
    </div>
    <div
      v-for="(freq, i) in bands"
      :key="freq"
      class="geq__band"
    >
      <div class="geq__value" :class="{ 'geq__value--zero': values[i] === 0 }">{{ format(values[i] ?? 0) }}</div>
      <div
        class="geq__track"
        role="slider"
        tabindex="0"
        :aria-label="`${FREQ_LABEL[freq] ?? freq} Hz`"
        :aria-valuemin="MIN"
        :aria-valuemax="MAX"
        :aria-valuenow="values[i] ?? 0"
        :aria-valuetext="`${format(values[i] ?? 0)} dB`"
        @pointerdown="onPointerDown($event, i)"
        @pointermove="onPointerMove($event, i)"
        @pointerup="onPointerUp($event, i)"
        @pointercancel="onPointerUp($event, i)"
        @wheel="onWheel($event, i)"
        @keydown="onKey($event, i)"
      >
        <div class="geq__rail"></div>
        <div class="geq__zero"></div>
        <div class="geq__thumb" :style="{ top: `${thumbY(values[i] ?? 0)}px` }"></div>
      </div>
      <div class="geq__freq">{{ FREQ_LABEL[freq] ?? freq }}</div>
    </div>
  </div>
</template>

<style scoped>
.geq {
  display: flex;
  align-items: flex-start;
  gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  overflow-x: auto;
}

.geq__banner {
  position: relative;
  /* The dB scale marks the TRACK, which sits below the value row (11px text
     + 4px gap); without this the labels float above their grid lines. */
  margin-top: 19px;
  height: 178px;
  width: 44px;
  color: var(--muted);
  font-size: 10px;
  white-space: nowrap;
  flex: 0 0 auto;
}

.geq__db {
  position: absolute;
  right: 0;
}

.geq__db--top {
  top: 22px;
}

.geq__db--mid {
  top: 91px;
  transform: translateY(-50%);
}

.geq__db--bottom {
  top: 166px;
  transform: translateY(-50%);
}

.geq__band {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  flex: 1 1 0;
  min-width: 44px;
}

.geq__value {
  font-variant-numeric: tabular-nums;
  font-size: 11px;
  color: var(--ink);
  white-space: nowrap;
}

.geq__value--zero {
  color: var(--muted);
}

.geq__track {
  position: relative;
  width: 28px;
  height: 178px;
  touch-action: none;
  cursor: ns-resize;
  outline: none;
}

.geq__track:focus-visible .geq__thumb {
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 35%, transparent);
}

.geq__rail {
  position: absolute;
  left: 50%;
  top: 14px;
  bottom: 14px;
  width: 3px;
  transform: translateX(-50%);
  border-radius: 2px;
  background: var(--line-strong);
  opacity: 0.55;
}

.geq__zero {
  position: absolute;
  left: calc(50% - 9px);
  right: calc(50% - 9px);
  top: 91px;
  height: 1px;
  background: var(--line-strong);
  opacity: 0.7;
}

.geq__thumb {
  position: absolute;
  left: 50%;
  width: 18px;
  height: 18px;
  margin-left: -9px;
  margin-top: -9px;
  border-radius: 50%;
  background: var(--accent);
  border: 2px solid var(--surface-2);
  box-shadow: 0 1px 2px rgb(0 0 0 / 0.25);
  transition: top 0.04s linear;
}

.geq__freq {
  font-size: 10px;
  color: var(--muted);
  white-space: nowrap;
}

.geq--disabled {
  opacity: 0.6;
  pointer-events: none;
}
</style>
