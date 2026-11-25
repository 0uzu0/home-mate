<script setup lang="ts">
/**
 * Nodes and configuration push (M12a-1, 2026-11-15).
 *
 * One card for three jobs that only make sense together:
 *  1. what this node is called (the `actor=fleet:<name>` a peer will record);
 *  2. which peers exist, and whether they are reachable;
 *  3. sending this node's configuration to a chosen subset of them.
 *
 * Two defaults are user decisions, not taste: "include secrets" is OFF (a
 * credential over plain LAN HTTP is opt-in every time) and a dry run is one
 * click away so a push can be inspected before it writes anything anywhere.
 *
 * The per-target result table is the point of the card: a push is not "done",
 * it is "ok for these, failed for those, with these notes" — the outcome the
 * core already returns per peer.
 */
import { computed, onMounted, ref, watch } from 'vue'
import {
  deleteFleetNode,
  getFleet,
  getStatus,
  pingFleet,
  pushFleet,
  putFleetNode,
  setFleetSelfId,
  type FleetNode,
  type FleetPushResult,
  type FleetView,
  type Status,
} from '../../api'
import { t } from '../../i18n'
import { formatRelative } from '../../format'
import { useResource } from '../../hooks/useResource'
import { useSave } from '../../hooks/useSave'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'
import ConfirmDialog from '../ConfirmDialog.vue'
import EmptyState from '../EmptyState.vue'
import AppIcon from '../AppIcon.vue'
import LoadingBlock from '../LoadingBlock.vue'
import ErrorNotice from '../ErrorNotice.vue'

const fleetRes = useResource((signal) => getFleet({ signal }), { timeoutMs: 6000, retries: 1 })
const fleet = computed(() => fleetRes.data.value)
const loadError = computed(() => fleetRes.error.value)
const loading = computed(() => fleetRes.loading.value)

const { busy, message, tone, run } = useSave()

// Identity
const selfId = ref('')
const selfIdBaseline = ref<string | null>(null)
const selfIdDirty = computed(
  () => selfIdBaseline.value !== null && selfId.value.trim() !== selfIdBaseline.value,
)

// Add-peer form
const newId = ref('')
const newUrl = ref('')
const newToken = ref('')

// Probe results keyed by peer id (a probe is per-peer; a failed one must not
// erase the others).
const probes = ref<Record<string, { ok: boolean; text: string }>>({})

// Push form. Two explicit actions instead of a "dry run" switch: with the
// switch on, the only visible button said 出计划 and there was no 下发 to be
// found (user report 2026-11-15). The safe action stays first, the writing one
// is always visible, and the real send asks for confirmation when it carries
// credentials.
const targets = ref<string[]>([])
const includeSecrets = ref(false)
const pushResult = ref<FleetPushResult | null>(null)
const confirmingPush = ref(false)

const pendingRemove = ref<FleetNode | null>(null)

const vault = ref<Status['vault'] | null>(null)

watch(
  () => fleetRes.data.value,
  (next) => {
    if (!next) return
    if (!selfIdDirty.value) {
      selfId.value = next.self_id
      selfIdBaseline.value = next.self_id
    }
    const ids = new Set(next.nodes.map((n) => n.id))
    targets.value = targets.value.filter((id) => ids.has(id))
  },
  { immediate: true },
)

async function load(): Promise<void> {
  void loadVault()
  await fleetRes.load()
}

async function loadVault(): Promise<void> {
  try {
    const status = await getStatus()
    vault.value = status.vault ?? null
  } catch {
    // The vault line is informational; the card's real job is the peers.
    vault.value = null
  }
}

onMounted(() => {
  void load()
})

async function reloadSelfId(): Promise<void> {
  await load()
  if (loadError.value || !fleet.value) return
  selfId.value = fleet.value.self_id
  selfIdBaseline.value = fleet.value.self_id
}

async function saveSelfId(): Promise<void> {
  if (!fleet.value || loadError.value || loading.value || !selfIdDirty.value) return
  await run(async () => {
    const updated = await setFleetSelfId(selfId.value.trim())
    fleetRes.data.value = updated
    selfId.value = updated.self_id
    selfIdBaseline.value = updated.self_id
    return t('fleetSelfIdSaved')
  })
}

async function addNode(): Promise<void> {
  const id = newId.value.trim()
  if (!id) return
  await run(async () => {
    const body: { base_url: string; token?: string } = { base_url: newUrl.value.trim() }
    if (newToken.value.trim()) body.token = newToken.value.trim()
    await putFleetNode(id, body)
    newId.value = ''
    newUrl.value = ''
    newToken.value = ''
    await load()
    return t('fleetNodeAdded', { id })
  })
}

async function removeNode(): Promise<void> {
  const node = pendingRemove.value
  pendingRemove.value = null
  if (!node) return
  await run(async () => {
    await deleteFleetNode(node.id)
    delete probes.value[node.id]
    await load()
    return t('fleetRemoved', { id: node.id })
  })
}

async function probe(ids?: string[]): Promise<void> {
  const wanted = ids ?? (fleet.value?.nodes ?? []).map((n) => n.id)
  if (!wanted.length) return
  await run(async () => {
    const out = await pingFleet(wanted)
    for (const r of out.results) {
      probes.value[r.id] = r.ok
        ? {
            ok: true,
            text: t('fleetPingOk', {
              id: r.id,
              build: String(r.build ?? '—'),
              ms: String(r.latency_ms ?? '—'),
            }),
          }
        : { ok: false, text: t('fleetPingFailed', { id: r.id, error: r.error ?? '—' }) }
    }
    await load()
    return t('fleetPingAll') + `: ${out.ok}/${out.results.length}`
  })
}

function toggleTarget(id: string): void {
  targets.value = targets.value.includes(id)
    ? targets.value.filter((x) => x !== id)
    : [...targets.value, id]
}

async function sendNow(dry: boolean): Promise<void> {
  confirmingPush.value = false
  if (!targets.value.length) {
    message.value = t('fleetPushNoTargets')
    tone.value = 'error'
    return
  }
  await run(async () => {
    const out = await pushFleet({
      targets: targets.value,
      include_secrets: includeSecrets.value,
      dry_run: dry,
    })
    pushResult.value = out
    await load()
    return out.dry_run
      ? t('fleetPushPlanned', { n: String(out.targets.length) })
      : t('fleetPushDone', { ok: String(out.ok), failed: String(out.failed) })
  })
}

/// A result line must never be able to blank the card: the core is supposed to
/// send `applied`/`warnings` on every target, but a single missing field used
/// to unmount the whole panel (2026-11-15).
function joined(list: string[] | undefined | null): string {
  return list && list.length ? list.join(' · ') : '—'
}

function askPush(): void {
  if (!targets.value.length) {
    message.value = t('fleetPushNoTargets')
    tone.value = 'error'
    return
  }
  // Carrying credentials over plain HTTP gets one confirmation; a plain
  // configuration push does not (it is reversible by pushing again).
  if (includeSecrets.value) {
    confirmingPush.value = true
    return
  }
  void sendNow(false)
}

function sourceLabel(source: FleetView['self_id_source']): string {
  switch (source) {
    case 'config':
      return t('fleetSelfIdSourceConfig')
    case 'env':
      return t('fleetSelfIdSourceEnv')
    case 'device':
      return t('fleetSelfIdSourceDevice')
    default:
      return t('fleetSelfIdSourceDefault')
  }
}

function nodeState(node: FleetNode): string {
  if (node.last_ok_ms) return t('fleetLastOk', { time: formatRelative(node.last_ok_ms) })
  return t('fleetNever')
}

function vaultSealed(): string {
  const n = Number(vault.value?.sealed ?? 0)
  const plain = Number(vault.value?.plaintext ?? 0)
  const bad = Number(vault.value?.undecryptable ?? 0)
  const parts = [t('vaultSealed', { n: String(n) })]
  if (plain) parts.push(t('vaultPlaintext', { n: String(plain) }))
  if (bad) parts.push(t('vaultUndecryptable', { n: String(bad) }))
  return parts.join(' · ')
}
</script>

<template>
  <Panel icon="package" :title="t('fleetTitle')" :hint="t('fleetHint')">
    <template #actions>
      <button type="button" class="btn" :disabled="loading || busy" @click="load">
        <AppIcon name="reload" :size="13" /> {{ loading ? t('loading') : t('refresh') }}
      </button>
    </template>

    <ErrorNotice v-if="loadError" :error="loadError" :stale="fleetRes.stale.value" :at="fleetRes.updatedAt.value" @retry="load()" />
    <LoadingBlock v-if="loading && !fleet" :label="t('loading')" />
    <template v-else-if="fleet">
      <!-- This node's identity -->
      <section class="fleet__block">
        <div class="fleet__row">
          <TextInput
            v-model="selfId"
            :disabled="busy || !fleet"
            :label="t('fleetSelfId')"
            :hint="t('fleetSelfIdHint')"
            placeholder="dot2-d"
            wide
          />
          <button
            type="button"
            class="btn"
            :disabled="busy || loading || !fleet || !!loadError || !selfIdDirty"
            @click="saveSelfId"
          >
            {{ t('save') }}
          </button>
          <button v-if="selfIdDirty" type="button" class="btn" :disabled="busy || loading" @click="reloadSelfId">{{ t('configReloadDraft') }}</button>
        </div>
        <p v-if="fleet" class="fleet__meta">
          {{ t('fleetSelfIdSource', { source: sourceLabel(fleet.self_id_source) }) }}
          <span class="mono"> · {{ fleet.push_actor }}</span>
        </p>
      </section>

      <!-- Peers -->
      <section class="fleet__block">
        <h3 class="fleet__h3">{{ t('fleetAddTitle') }}</h3>
        <div class="fleet__form">
          <TextInput v-model="newId" :label="t('fleetNodeId')" :placeholder="t('fleetNodeIdPlaceholder')" />
          <TextInput
            v-model="newUrl"
            :label="t('fleetNodeUrl')"
            :hint="t('fleetNodeUrlHint')"
            placeholder="http://192.168.1.69:8930"
            wide
          />
          <PasswordField
            v-model="newToken"
            :label="t('fleetNodeToken')"
            :hint="t('fleetNodeTokenHint')"
            :is-set="false"
            placeholder="hm1_…"
          />
          <button type="button" class="btn btn--primary" :disabled="busy || !newId.trim()" @click="addNode">
            <AppIcon name="add" :size="13" /> {{ t('fleetAdd') }}
          </button>
        </div>
      </section>

      <EmptyState v-if="fleet && !fleet.nodes.length" :title="t('fleetNoNodes')" />

      <div v-else-if="fleet" class="fleet__table-wrap">
        <table class="fleet__table">
        <thead>
          <tr>
            <th />
            <th>{{ t('fleetNodeId') }}</th>
            <th>{{ t('fleetNodeUrl') }}</th>
            <th>{{ t('fleetNodeToken') }}</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="node in fleet.nodes" :key="node.id">
            <td>
              <input
                type="checkbox"
                :checked="targets.includes(node.id)"
                :aria-label="node.id"
                @change="toggleTarget(node.id)"
              />
            </td>
            <td class="mono">{{ node.id }}</td>
            <td class="mono">
              {{ node.base_url }}
              <div class="fleet__sub">
                <template v-if="node.last_ok_ms">{{ nodeState(node) }}</template>
                <template v-else>{{ t('fleetNever') }}</template>
                <template v-if="node.last_error">
                  · <span class="fleet__err">{{ t('fleetLastError', { error: node.last_error }) }}</span>
                </template>
              </div>
              <div v-if="probes[node.id]" class="fleet__sub" :class="probes[node.id].ok ? 'fleet__ok' : 'fleet__err'">
                {{ probes[node.id].text }}
              </div>
            </td>
            <td>
              <Pill :tone="node.token_set ? 'ok' : 'warn'" size="sm">
                {{ node.token_set ? t('apiKeySet') : t('apiKeyUnset') }}
              </Pill>
            </td>
            <td class="fleet__actions">
              <button type="button" class="btn" :disabled="busy" @click="probe([node.id])">
                {{ t('fleetPingOne') }}
              </button>
              <button type="button" class="btn" :disabled="busy" @click="pendingRemove = node">
                <AppIcon name="remove" :size="13" /> {{ t('fleetRemove') }}
              </button>
            </td>
          </tr>
        </tbody>
        </table>
      </div>

      <div v-if="fleet && fleet.nodes.length" class="fleet__row">
        <button type="button" class="btn" :disabled="busy" @click="probe()">
          <AppIcon name="probes" :size="13" /> {{ t('fleetPingAll') }}
        </button>
      </div>

      <!-- Push -->
      <section v-if="fleet && fleet.nodes.length" class="fleet__block">
        <h3 class="fleet__h3">{{ t('fleetPushTitle') }}</h3>
        <p class="fleet__hint">{{ t('fleetPushHint') }}</p>
        <Toggle v-model="includeSecrets" :label="t('fleetPushIncludeSecrets')" :hint="t('fleetPushIncludeSecretsHint')" />
        <div class="fleet__row">
          <button type="button" class="btn" :disabled="busy" @click="sendNow(true)">
            <AppIcon name="info" :size="13" />
            {{ t('fleetPushPlan') }}
          </button>
          <button type="button" class="btn btn--primary" :disabled="busy" @click="askPush">
            <AppIcon name="upload" :size="13" />
            {{ t('fleetPushRun') }}
          </button>
          <span class="fleet__hint">
            {{ targets.length ? targets.join(' · ') : t('fleetPushNoTargets') }}
          </span>
        </div>

        <div v-if="pushResult" class="fleet__table-wrap">
          <table class="fleet__table">
          <thead>
            <tr>
              <th>{{ t('fleetNodeId') }}</th>
              <th>{{ pushResult.dry_run ? t('fleetPushPlan') : t('fleetPushRun') }}</th>
              <th>{{ pushResult.dry_run ? t('fleetPushWouldSend') : t('fleetPushBlocks') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="r in pushResult.results" :key="r.id">
              <td class="mono">{{ r.id }}</td>
              <td>
                <Pill :tone="r.ok ? 'ok' : 'warn'" size="sm">
                  {{ r.ok ? t('fleetPushTargetOk') : t('fleetPushTargetFailed') }}
                </Pill>
                <div v-if="r.actor" class="fleet__sub mono">
                  {{ t('fleetPushActor', { actor: r.actor }) }}
                </div>
                <div v-if="!r.ok && r.error" class="fleet__sub fleet__err">{{ r.error }}</div>
              </td>
              <td>
                <div class="mono fleet__sub">{{ joined(r.applied) }}</div>
                <div v-if="r.warnings && r.warnings.length" class="fleet__sub">
                  {{ t('fleetPushWarnings') }}: {{ joined(r.warnings) }}
                </div>
              </td>
            </tr>
          </tbody>
          </table>
        </div>
      </section>

      <!-- Credential storage, from /status.vault (counts only) -->
      <p v-if="vault" class="fleet__meta">
        <strong>{{ t('vaultTitle') }}</strong> · {{ vaultSealed() }}
        <template v-if="vault.key_path">
          · <span class="mono">{{ t('vaultKeyPath', { path: String(vault.key_path) }) }}</span>
        </template>
      </p>
      <p v-if="vault && Number(vault.undecryptable ?? 0) > 0" class="msg msg--error">
        {{ t('vaultUndecryptableHint') }}
      </p>

      <p v-if="message" class="msg" :class="`msg--${tone}`">{{ message }}</p>
    </template>

    <ConfirmDialog
      v-if="confirmingPush"
      :title="t('fleetPushConfirmTitle', { n: String(targets.length) })"
      :message="`${t('fleetPushConfirmBody')}\n${targets.join(' · ')}`"
      :confirm-label="t('fleetPushRun')"
      :cancel-label="t('cancel')"
      :busy="busy"
      :busy-label="t('saving')"
      tone="danger"
      @confirm="sendNow(false)"
      @cancel="confirmingPush = false"
    />

    <ConfirmDialog
      v-if="pendingRemove"
      :title="t('fleetRemoveConfirmTitle', { id: pendingRemove.id })"
      :message="t('fleetRemoveConfirmBody')"
      :confirm-label="t('fleetRemove')"
      :cancel-label="t('cancel')"
      :busy="busy"
      :busy-label="t('saving')"
      tone="danger"
      @confirm="removeNode"
      @cancel="pendingRemove = null"
    />
  </Panel>
</template>

<style scoped>
.fleet__block {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding-top: var(--sp-2);
  border-top: 1px solid var(--line);
}

.fleet__h3 {
  font-size: var(--fs-sm);
  font-weight: 650;
  color: var(--ink);
}

.fleet__hint {
  font-size: var(--fs-xs);
  color: var(--muted);
  max-width: 75ch;
}

.fleet__row {
  display: flex;
  align-items: flex-end;
  gap: var(--sp-3);
  flex-wrap: wrap;
}

.fleet__form {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr));
  gap: var(--sp-3);
  align-items: end;
}

.fleet__table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-sm);
}

.fleet__table th {
  text-align: left;
  font-weight: 600;
  color: var(--muted);
  font-size: var(--fs-xs);
  padding: var(--sp-1) var(--sp-2);
  border-bottom: 1px solid var(--line);
}

.fleet__table td {
  padding: var(--sp-2);
  border-bottom: 1px solid var(--line);
  vertical-align: top;
}

.fleet__table-wrap {
  width: 100%;
  overflow-x: auto;
  -webkit-overflow-scrolling: touch;
}

.fleet__sub {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.fleet__ok {
  color: var(--ok);
}

.fleet__err {
  color: var(--danger);
}

.fleet__actions {
  display: flex;
  gap: var(--sp-2);
  justify-content: flex-end;
}

.fleet__meta {
  font-size: var(--fs-xs);
  color: var(--muted);
}

.msg {
  font-size: var(--fs-sm);
}

.msg--ok {
  color: var(--ok);
}

.msg--error {
  color: var(--danger);
}
</style>
