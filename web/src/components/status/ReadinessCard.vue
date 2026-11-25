<script setup lang="ts">
/**
 * First-run readiness (2026-11-14, user decision): the status page is the
 * landing page, and until now a fresh install showed "no device" with no way
 * forward. This card answers the three questions that actually decide whether
 * the thing can hear you:
 *
 *   1. the three services are configured (LLM / ASR / TTS),
 *   2. a device is online,
 *   3. it has been woken at least once since this core started.
 *
 * The wake check reads the device's own counters (spotter wakes + per-word
 * hits, both from the device report) — a fact, not a self-declared "done".
 * It is a checklist, not a wizard: every line links to the page that owns it.
 */
import { computed, onMounted } from 'vue'
import { RouterLink } from 'vue-router'
import { getServices } from '../../api'
import { errorDetail } from '../../format'
import { t } from '../../i18n'
import { useResource } from '../../hooks/useResource'
import type { GatewayDevice, ModeDevice, ModeSnapshot } from '../../api'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import AppIcon from '../AppIcon.vue'

const props = defineProps<{
  mode: ModeSnapshot | null
  device: ModeDevice | null
  gatewayDevice?: GatewayDevice | null
}>()

const services = useResource((signal) => getServices({ signal }), { timeoutMs: 6000, retries: 1 })
// `useResource` only loads when asked; without this the row sat on
// "正在读取…" forever (user report 2026-11-14), because nothing ever called
// `load()` and a failed read looked identical to one that never happened.
onMounted(() => {
  void services.load()
})

/** Services configured but whose declared env key is missing. */
const missingKeys = computed(() => {
  const s = services.data.value
  if (!s) return [] as string[]
  const names: Array<[string, string]> = [
    ['llm', 'LLM'],
    ['asr', 'ASR'],
    ['tts', 'TTS'],
  ]
  return names
    .filter(([key]) => {
      const svc = s[key as keyof typeof s]
      // Only a service that DECLARES an env key needs one; a keyless local
      // provider is a legitimate setup and must not be flagged.
      return !!svc?.configured && !!svc?.api_key_env && svc?.api_key_set === false
    })
    .map(([, label]) => label)
})

const serviceReady = computed(() => {
  const s = services.data.value
  if (!s) return null
  return !!(s.llm?.configured && s.asr?.configured && s.tts?.configured) && missingKeys.value.length === 0
})

const deviceOnline = computed(() => {
  if (!props.device) return null
  if (props.gatewayDevice && props.gatewayDevice.stable_id === props.device.device) {
    return props.gatewayDevice.online ? true : false
  }
  return props.mode?.devices.some((d) => d.device === props.device?.device) ? true : null
})

/** The device's own wake evidence since this core started. */
const wakeCount = computed(() => {
  const d = props.device
  if (!d) return 0
  const kws = d.kws?.wakes ?? 0
  const words = (d.reflex?.words ?? []).reduce(
    (n, w) => n + (w.hits ?? 0) + (w.text_hits ?? 0),
    0,
  )
  return kws + words
})

interface Row {
  ok: boolean | null
  label: string
  detail: string
  to: string
  cta: string
}

const rows = computed<Row[]>(() => [
  {
    ok: serviceReady.value,
    label: t('readyServices'),
    detail:
      serviceReady.value === null
        ? services.error.value
          ? t('readyServicesError', { detail: errorDetail(services.error.value) })
          : t('readyChecking')
        : serviceReady.value
          ? t('readyServicesOk')
          : missingKeys.value.length
            ? t('readyServicesNoKey', { services: missingKeys.value.join(' / ') })
            : t('readyServicesMissing'),
    to: '/services',
    cta: t('navServices'),
  },
  {
    ok: deviceOnline.value,
    label: t('readyDevice'),
    detail:
      deviceOnline.value === true
        ? `${props.device?.device ?? ''} · ${t('readyDeviceOk')}`
        : deviceOnline.value === false
          ? `${props.device?.device ?? ''} · ${t('offline')}`
          : t('readyDeviceMissing'),
    to: '/devices',
    cta: t('navDevices'),
  },
  {
    ok: wakeCount.value > 0 ? true : null,
    label: t('readyWake'),
    detail: wakeCount.value > 0 ? t('readyWakeOk') : t('readyWakeMissing'),
    to: '/interaction',
    cta: t('navInteraction'),
  },
])

const allOk = computed(() => rows.value.every((r) => r.ok === true))
</script>

<template>
  <Panel
    icon="check"
    :title="t('readyTitle')"
    :hint="t('readyHint')"
    :tone="allOk ? 'ok' : 'default'"
  >
    <ul class="ready">
      <li v-for="row in rows" :key="row.label" class="ready__row">
        <Pill size="sm" :tone="row.ok === true ? 'ok' : row.ok === null ? 'warn' : 'danger'">
          {{ row.ok === true ? '✓' : row.ok === null ? '…' : '✗' }}
        </Pill>
        <span class="ready__label">{{ row.label }}</span>
        <span class="ready__detail">{{ row.detail }}</span>
        <RouterLink class="ready__link" :to="row.to" :title="row.cta" :aria-label="row.cta">
          <AppIcon name="arrowUpRight" :size="13" />
        </RouterLink>
      </li>
    </ul>
    <p v-if="allOk" class="ready__done">{{ t('readyDone') }}</p>
  </Panel>
</template>

<style scoped>
.ready {
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.ready__row {
  display: grid;
  grid-template-columns: auto 11rem 1fr auto;
  gap: var(--sp-2);
  align-items: center;
  font-size: var(--fs-sm);
}

@media (max-width: 640px) {
  .ready__row {
    grid-template-columns: auto 1fr auto;
  }
  .ready__detail {
    grid-column: 1 / -1;
    order: 1;
  }
}

.ready__label {
  font-weight: 500;
}

.ready__detail {
  color: var(--muted);
}

.ready__link {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 1.55rem;
  height: 1.55rem;
  border: 1px solid var(--line-strong);
  border-radius: var(--r-sm);
  color: var(--accent);
  background: var(--surface-2);
  text-decoration: none;
  justify-self: end;
}

.ready__link:hover {
  border-color: var(--accent);
  background: var(--accent-soft);
}

.ready__done {
  margin-top: var(--sp-2);
  color: var(--ok);
  font-size: var(--fs-sm);
}
</style>
