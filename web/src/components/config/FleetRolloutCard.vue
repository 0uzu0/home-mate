<script setup lang="ts">
import { computed, onMounted, ref, shallowRef, watch } from 'vue'
import { getFleet, type FleetView } from '../../api'
import { ApiError } from '../../api/client'
import {
  commitFleet,
  getConfigState,
  getFleetRun,
  issueHouseholdInvitation,
  joinHousehold,
  newConfigIdentity,
  preflightFleet,
  retryFleetRun,
  rollbackFleetTarget,
  type ConfigPackage,
  type ConfigState,
  type ConfigValue,
  type FleetPreflightResponse,
  type FleetRollbackResponse,
  type FleetRunStatus,
  type FleetRunTarget,
  type FleetTargetPlan,
  type HouseholdInvitation,
  type JoinHouseholdResponse,
} from '../../api/configV2'
import { formatTime } from '../../format'
import { t } from '../../i18n'
import ErrorNotice from '../ErrorNotice.vue'
import LoadingBlock from '../LoadingBlock.vue'
import Panel from '../Panel.vue'
import Pill from '../Pill.vue'

const ACTIVE_RUN_KEY = 'hm.fleet.activeRunId'

function readStoredRunId(): string {
  try {
    return typeof localStorage !== 'undefined' ? localStorage.getItem(ACTIVE_RUN_KEY) ?? '' : ''
  } catch {
    return ''
  }
}

function writeStoredRunId(id: string): void {
  try {
    if (typeof localStorage === 'undefined') return
    if (id) localStorage.setItem(ACTIVE_RUN_KEY, id)
    else localStorage.removeItem(ACTIVE_RUN_KEY)
  } catch {
    // Ignore storage errors in restricted environments.
  }
}

const configState = shallowRef<ConfigState | null>(null)
const fleet = shallowRef<FleetView | null>(null)
const loading = ref(false)
const loadError = ref<unknown>(null)
const actionError = ref<unknown>(null)
const conflictDraftNotice = ref(false)
const busy = ref(false)

// Household invitation issue
const inviteNodeId = ref('')
const inviteTtlMinutes = ref(10)
const issuedInvitation = shallowRef<HouseholdInvitation | null>(null)

// Household join
const joinInput = ref('')
const joinIssuerNode = ref('')
const joinIssuerUrl = ref('')
const joinIssuerToken = ref('')
const joinResult = shallowRef<JoinHouseholdResponse | null>(null)

// Sequential fleet rollout
const selectedTargets = ref<string[]>([])
const sanitizeFamily = ref(false)
const packageDraft = ref('')
const preflightResult = shallowRef<FleetPreflightResponse | null>(null)
const activeRun = shallowRef<FleetRunStatus | null>(null)
const resumeRunId = ref(readStoredRunId())
const pendingRunId = ref<string | null>(null)

watch([packageDraft, selectedTargets, sanitizeFamily], () => {
  pendingRunId.value = null
})

// Per-target rollback preview/result keyed by node_id
const rollbackStates = ref<Record<string, FleetRollbackResponse>>({})

const enrolled = computed(() => configState.value?.target.enrolled === true)
const selfNodeId = computed(() => configState.value?.target.node_id || fleet.value?.self_id || '')

const availableNodes = computed(() => {
  const ids = new Set<string>()
  if (selfNodeId.value) ids.add(selfNodeId.value)
  for (const n of fleet.value?.nodes ?? []) ids.add(n.id)
  return [...ids]
})

function sortDot2dFirst(nodes: string[]): string[] {
  const unique = [...new Set(nodes.map(s => s.trim()).filter(Boolean))]
  const hasDot2d = unique.includes('dot2-d')
  const rest = unique.filter(id => id !== 'dot2-d')
  return hasDot2d ? ['dot2-d', ...rest] : rest
}

const orderedTargets = computed(() => sortDot2dFirst(selectedTargets.value))

interface InvitationPreview {
  token: string
  issuer_node_id?: string
  expected_node_id?: string
  household_id?: string
  expires_ms?: number
}

const parsedJoin = computed<InvitationPreview | null>(() => {
  const raw = joinInput.value.trim()
  if (!raw) return null
  if (raw.startsWith('{')) {
    try {
      const obj = JSON.parse(raw) as Record<string, unknown>
      const token = typeof obj.token === 'string' ? obj.token.trim() : ''
      if (!token) return null
      return {
        token,
        issuer_node_id: typeof obj.issuer_node_id === 'string' ? obj.issuer_node_id : undefined,
        expected_node_id: typeof obj.expected_node_id === 'string' ? obj.expected_node_id : undefined,
        household_id: typeof obj.household_id === 'string' ? obj.household_id : undefined,
        expires_ms: typeof obj.expires_ms === 'number' ? obj.expires_ms : undefined,
      }
    } catch {
      return null
    }
  }
  return { token: raw }
})

const issuedPayloadJson = computed(() => {
  if (!issuedInvitation.value) return ''
  return JSON.stringify(
    {
      token: issuedInvitation.value.token,
      issuer_node_id: selfNodeId.value || undefined,
      expected_node_id: issuedInvitation.value.expected_node_id,
      household_id: issuedInvitation.value.household_id,
      expires_ms: issuedInvitation.value.expires_ms,
    },
    null,
    2,
  )
})

function buildStarterPackage(): ConfigPackage {
  const st = configState.value
  return {
    format: 'home-mate-config',
    version: 2,
    package_id: newConfigIdentity(),
    package_revision: 1,
    created_ms: Date.now(),
    purpose: 'family_policy',
    source: {
      node_id: st?.target.node_id || selfNodeId.value || 'dot2-d',
      config_revision: st?.config_revision ?? 1,
      core_version: st?.core_version ?? '0.1.0',
      schema_version: st?.schema_version ?? 1,
    },
    target: {
      household_id: st?.target.household_id ?? '',
    },
    compatibility: {
      core_api_schema: 2,
      gateway_protocol_min: 0,
      gateway_protocol_max: 0,
      required_capabilities: [],
    },
    change_reason: 'Sequential household family_policy rollout',
    blocks: [
      {
        name: 'mode',
        schema_version: 1,
        operations: [{ action: 'set', path: 'mode.follow_window_enabled', value: true }],
      },
    ],
    bindings: [],
  }
}

function loadStarterPackage(): void {
  packageDraft.value = JSON.stringify(buildStarterPackage(), null, 2)
  preflightResult.value = null
  conflictDraftNotice.value = false
}

async function load(): Promise<void> {
  loading.value = true
  loadError.value = null
  try {
    const [st, fl] = await Promise.all([getConfigState(), getFleet()])
    configState.value = st
    fleet.value = fl
    if (!packageDraft.value.trim()) {
      packageDraft.value = JSON.stringify(buildStarterPackage(), null, 2)
    }
    const storedRunId = resumeRunId.value.trim()
    if (storedRunId && !activeRun.value) {
      try {
        activeRun.value = await getFleetRun(storedRunId)
      } catch {
        // Ignore stale run id on initial auto-resume; user can still query manually.
      }
    }
  } catch (err) {
    loadError.value = err
  } finally {
    loading.value = false
  }
}

onMounted(() => {
  void load()
})

function toggleTarget(id: string): void {
  selectedTargets.value = selectedTargets.value.includes(id)
    ? selectedTargets.value.filter(x => x !== id)
    : [...selectedTargets.value, id]
  preflightResult.value = null
}

function isConflictError(err: unknown): boolean {
  return err instanceof ApiError && (err.status === 409 || err.code === 'conflict' || err.detail.includes('CONFIG_CONFLICT'))
}

async function onIssueInvitation(): Promise<void> {
  const expected = inviteNodeId.value.trim()
  if (!expected || busy.value) return
  busy.value = true
  actionError.value = null
  try {
    const ttlMs = Math.max(1, Number(inviteTtlMinutes.value || 10)) * 60 * 1000
    issuedInvitation.value = await issueHouseholdInvitation({ expected_node_id: expected, ttl_ms: ttlMs })
  } catch (err) {
    actionError.value = err
  } finally {
    busy.value = false
  }
}

async function onJoinHousehold(): Promise<void> {
  const preview = parsedJoin.value
  if (!preview || busy.value) return
  busy.value = true
  actionError.value = null
  try {
    const issuerNode = joinIssuerNode.value.trim() || preview.issuer_node_id
    const issuerUrl = joinIssuerUrl.value.trim()
    const issuerToken = joinIssuerToken.value.trim()
    joinResult.value = await joinHousehold({
      token: preview.token,
      ...(issuerNode ? { issuer_node_id: issuerNode } : {}),
      ...(issuerUrl ? { issuer_base_url: issuerUrl } : {}),
      ...(issuerToken ? { issuer_token: issuerToken } : {}),
    })
    await load()
  } catch (err) {
    actionError.value = err
  } finally {
    busy.value = false
  }
}

function parsePackageDraft(): ConfigPackage {
  return JSON.parse(packageDraft.value) as ConfigPackage
}

async function onPreflightFleet(): Promise<void> {
  if (!orderedTargets.value.length || busy.value) return
  busy.value = true
  actionError.value = null
  conflictDraftNotice.value = false
  try {
    const pkg = parsePackageDraft()
    preflightResult.value = await preflightFleet({
      package: pkg,
      targets: orderedTargets.value,
      sanitize_family: sanitizeFamily.value,
    })
  } catch (err) {
    if (isConflictError(err)) conflictDraftNotice.value = true
    actionError.value = err
  } finally {
    busy.value = false
  }
}

async function onCommitFleet(): Promise<void> {
  if (!orderedTargets.value.length || busy.value) return
  busy.value = true
  actionError.value = null
  conflictDraftNotice.value = false
  try {
    const pkg = preflightResult.value?.package ?? parsePackageDraft()
    const plans: FleetTargetPlan[] | undefined = preflightResult.value?.targets
      .filter(item => item.ok && item.plan_id && item.plan_hash && item.expected_revision !== undefined)
      .map(item => ({
        node_id: item.node_id,
        plan_id: item.plan_id!,
        plan_hash: item.plan_hash!,
        expected_revision: item.expected_revision!,
      }))
    const runId = pendingRunId.value ?? newConfigIdentity()
    pendingRunId.value = runId
    writeStoredRunId(runId)
    resumeRunId.value = runId
    const run = await commitFleet({
      run_id: runId,
      package: pkg,
      targets: orderedTargets.value,
      sanitize_family: sanitizeFamily.value,
      ...(plans?.length ? { plans } : {}),
    })
    activeRun.value = run
    pendingRunId.value = null
    rollbackStates.value = {}
  } catch (err) {
    if (isConflictError(err)) {
      conflictDraftNotice.value = true
      preflightResult.value = null
      pendingRunId.value = null
    }
    actionError.value = err
  } finally {
    busy.value = false
  }
}

async function onLoadRun(): Promise<void> {
  const id = resumeRunId.value.trim()
  if (!id || busy.value) return
  busy.value = true
  actionError.value = null
  try {
    activeRun.value = await getFleetRun(id)
    writeStoredRunId(id)
  } catch (err) {
    actionError.value = err
  } finally {
    busy.value = false
  }
}

async function onRetryRun(): Promise<void> {
  if (!activeRun.value || busy.value) return
  busy.value = true
  actionError.value = null
  try {
    activeRun.value = await retryFleetRun(activeRun.value.run_id)
  } catch (err) {
    if (isConflictError(err)) conflictDraftNotice.value = true
    actionError.value = err
  } finally {
    busy.value = false
  }
}

async function onRollbackPreview(target: FleetRunTarget): Promise<void> {
  if (!activeRun.value || busy.value) return
  busy.value = true
  actionError.value = null
  try {
    const resp = await rollbackFleetTarget(activeRun.value.run_id, target.node_id, { commit: false })
    rollbackStates.value = { ...rollbackStates.value, [target.node_id]: resp }
  } catch (err) {
    actionError.value = err
  } finally {
    busy.value = false
  }
}

async function onRollbackCommit(target: FleetRunTarget): Promise<void> {
  if (!activeRun.value || busy.value) return
  const preview = rollbackStates.value[target.node_id]
  busy.value = true
  actionError.value = null
  try {
    const resp = await rollbackFleetTarget(activeRun.value.run_id, target.node_id, {
      expected_revision: preview?.preflight.expected_revision,
      commit: true,
    })
    rollbackStates.value = { ...rollbackStates.value, [target.node_id]: resp }
    activeRun.value = await getFleetRun(activeRun.value.run_id)
  } catch (err) {
    actionError.value = err
  } finally {
    busy.value = false
  }
}

function targetStateLabel(state: FleetRunTarget['state']): string {
  switch (state) {
    case 'pending':
      return t('cfgFleetStatePreflighted')
    case 'committing':
      return t('cfgFleetStateSavedPending')
    case 'applied':
      return t('cfgFleetStateAppliedVerified')
    case 'failed':
      return t('cfgFleetStateFailed')
    case 'skipped':
      return t('cfgFleetStateSkipped')
    case 'rolled_back':
      return t('cfgFleetStateRolledBack')
  }
}

function targetStateTone(state: FleetRunTarget['state']): 'ok' | 'warn' | 'muted' {
  if (state === 'applied') return 'ok'
  if (state === 'failed' || state === 'skipped') return 'warn'
  return 'muted'
}

function displayValue(value: ConfigValue | undefined): string {
  if (value === undefined) return '—'
  return typeof value === 'string' ? value || '""' : JSON.stringify(value)
}

const hasRetryableTargets = computed(() =>
  !!activeRun.value && (activeRun.value.state === 'failed' || activeRun.value.targets.some(item => item.state === 'failed' || item.state === 'skipped')),
)
</script>

<template>
  <Panel icon="package" :title="t('cfgFleetRolloutTitle')" :hint="t('cfgFleetRolloutHint')">
    <template #actions>
      <button type="button" class="btn" :disabled="loading || busy" @click="load">
        {{ loading ? t('loading') : t('refresh') }}
      </button>
    </template>

    <ErrorNotice v-if="loadError" :error="loadError" @retry="load" />
    <LoadingBlock v-else-if="!configState && loading" :label="t('loading')" />
    <ErrorNotice v-if="actionError" :error="actionError" @retry="load" />
    <p v-if="conflictDraftNotice" class="banner banner--warn" role="alert">{{ t('cfgFleetConflictDraft') }}</p>

    <template v-if="configState">
      <p v-if="!enrolled" class="muted">{{ t('cfgStandaloneEnrollNote') }}</p>

      <!-- Household invitations: Issue -->
      <section v-if="enrolled" class="card-section">
        <h3>{{ t('cfgInviteIssueTitle') }}</h3>
        <p class="muted">{{ t('cfgInviteIssueHint') }}</p>
        <div class="row">
          <label class="field">
            {{ t('cfgInviteExpectedNode') }}
            <input v-model="inviteNodeId" type="text" placeholder="dot2-e" />
          </label>
          <label class="field">
            {{ t('cfgInviteTtlMinutes') }}
            <input v-model.number="inviteTtlMinutes" type="number" min="1" max="1440" />
          </label>
          <button type="button" class="btn btn--primary" :disabled="busy || !inviteNodeId.trim()" @click="onIssueInvitation">
            {{ t('cfgInviteIssueAction') }}
          </button>
        </div>
        <div v-if="issuedInvitation" class="subsection">
          <p>
            <strong>{{ t('cfgInviteIssued') }}</strong> ·
            {{ t('cfgInviteExpectedNode') }}: <code>{{ issuedInvitation.expected_node_id }}</code> ·
            {{ t('cfgInviteGeneration') }}: <code>{{ issuedInvitation.membership_generation }}</code> ·
            {{ t('cfgInviteExpiresAt') }}: {{ formatTime(issuedInvitation.expires_ms) }}
          </p>
          <p>{{ t('cfgInviteToken') }}: <code class="hash">{{ issuedInvitation.token }}</code></p>
          <label class="field">
            {{ t('cfgInvitePayload') }}
            <textarea readonly rows="5" :value="issuedPayloadJson" />
          </label>
        </div>
      </section>

      <!-- Household invitations: Join -->
      <section class="card-section">
        <h3>{{ t('cfgJoinTitle') }}</h3>
        <p class="muted">{{ t('cfgJoinHint') }}</p>
        <label class="field">
          {{ t('cfgJoinTokenOrPayload') }}
          <textarea v-model="joinInput" rows="3" placeholder="token or { &quot;token&quot;: &quot;...&quot;, &quot;issuer_node_id&quot;: &quot;dot2-d&quot; }" />
        </label>
        <div v-if="parsedJoin" class="subsection">
          <strong>{{ t('cfgJoinPreviewTitle') }}</strong>
          <p class="muted">
            <template v-if="parsedJoin.issuer_node_id">{{ t('cfgJoinPreviewIssuer') }}: <code>{{ parsedJoin.issuer_node_id }}</code> · </template>
            <template v-if="parsedJoin.expected_node_id">{{ t('cfgJoinPreviewExpected') }}: <code>{{ parsedJoin.expected_node_id }}</code> · </template>
            <template v-if="parsedJoin.household_id">{{ t('cfgJoinPreviewHousehold') }}: <code>{{ parsedJoin.household_id }}</code> · </template>
            <template v-if="parsedJoin.expires_ms">{{ t('cfgJoinPreviewExpiry') }}: {{ formatTime(parsedJoin.expires_ms) }}</template>
          </p>
        </div>
        <div class="row">
          <label class="field">
            {{ t('cfgJoinIssuerNode') }}
            <input v-model="joinIssuerNode" type="text" placeholder="dot2-d" />
          </label>
          <label class="field">
            {{ t('cfgJoinIssuerUrl') }}
            <input v-model="joinIssuerUrl" type="text" placeholder="http://127.0.0.1:8930" />
          </label>
          <label class="field">
            {{ t('cfgJoinIssuerToken') }}
            <input v-model="joinIssuerToken" type="password" placeholder="hm1_..." />
          </label>
        </div>
        <div class="actions">
          <button type="button" class="btn btn--primary" :disabled="busy || !parsedJoin" @click="onJoinHousehold">
            {{ t('cfgJoinAction') }}
          </button>
        </div>
        <p v-if="joinResult" class="muted" role="status">
          {{ t('cfgJoinSuccess', { household: joinResult.household_id, node: joinResult.node_id, gen: String(joinResult.membership_generation) }) }}
        </p>
      </section>

      <!-- Sequential Fleet Rollout -->
      <section class="card-section">
        <h3>{{ t('cfgFleetSelectTitle') }}</h3>
        <p class="muted">{{ t('cfgFleetPreservedLocal') }}</p>
        <div>
          <strong>{{ t('cfgFleetSelectTargets') }}</strong>
          <div class="actions">
            <label v-for="nodeId in availableNodes" :key="nodeId" class="choose">
              <input type="checkbox" :checked="selectedTargets.includes(nodeId)" @change="toggleTarget(nodeId)" />
              <code>{{ nodeId }}</code>
            </label>
          </div>
          <p class="muted">
            {{ t('cfgFleetOrderPreview') }}:
            <code>{{ orderedTargets.length ? orderedTargets.join(' → ') : '—' }}</code>
          </p>
        </div>

        <label class="choose">
          <input v-model="sanitizeFamily" type="checkbox" />
          {{ t('cfgFleetSanitizeFamily') }}
        </label>

        <label class="field">
          {{ t('cfgFleetPackageJson') }}
          <textarea v-model="packageDraft" rows="8" spellcheck="false" />
        </label>

        <div class="actions">
          <button type="button" class="btn" :disabled="busy" @click="loadStarterPackage">{{ t('cfgFleetLoadSample') }}</button>
          <button type="button" class="btn" :disabled="busy || !orderedTargets.length || !packageDraft.trim()" @click="onPreflightFleet">
            {{ t('cfgFleetPreflightAction') }}
          </button>
          <button type="button" class="btn btn--primary" :disabled="busy || !orderedTargets.length || !packageDraft.trim() || (!!preflightResult && !preflightResult.all_ok)" @click="onCommitFleet">
            {{ t('cfgFleetCommitAction') }}
          </button>
        </div>

        <!-- Per-target preflight diff -->
        <div v-if="preflightResult" class="subsection">
          <p class="muted">
            {{ t('cfgFleetOrderPreview') }}: <code>{{ preflightResult.ordered_targets.join(' → ') }}</code>
          </p>
          <div v-for="target in preflightResult.targets" :key="target.node_id" class="target-card">
            <div class="target-head">
              <strong>{{ t('cfgFleetTargetDiff', { node: target.node_id }) }}</strong>
              <Pill :tone="target.ok && !(target.blocked?.length) ? 'ok' : 'warn'" size="sm">
                {{ target.ok ? t('cfgFleetTargetOk') : (target.error?.code || 'ERROR') }}
              </Pill>
            </div>
            <p v-if="!target.ok && target.error" class="muted">{{ target.error.code }}: {{ target.error.detail }}</p>
            <p v-if="target.ok" class="muted">
              <code>{{ target.plan_id }}</code> · {{ t('cfgRevision') }} {{ target.expected_revision }}
            </p>
            <div v-if="target.diff?.length" class="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>{{ t('cfgField') }}</th>
                    <th>{{ t('cfgFleetStored') }}</th>
                    <th>{{ t('cfgFleetDesired') }}</th>
                    <th>{{ t('cfgFleetEffective') }}</th>
                    <th>{{ t('cfgOrigin') }}</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="row in target.diff" :key="row.path">
                    <td><code>{{ row.path }}</code></td>
                    <td>{{ displayValue(row.before) }}</td>
                    <td>{{ displayValue(row.layer_after) }}</td>
                    <td>{{ displayValue(row.after) }}</td>
                    <td>{{ row.origin }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
            <p v-if="target.blocked?.length" role="alert">
              {{ t('cfgFleetTargetBlocked') }}: <code>{{ JSON.stringify(target.blocked) }}</code>
            </p>
          </div>
        </div>
      </section>

      <!-- Active / Resumed Sequential Run -->
      <section class="card-section">
        <h3>{{ t('cfgFleetRunTitle') }}</h3>
        <div class="row">
          <label class="field">
            {{ t('cfgFleetResumeLabel') }}
            <input v-model="resumeRunId" type="text" placeholder="uuid" />
          </label>
          <button type="button" class="btn" :disabled="busy || !resumeRunId.trim()" @click="onLoadRun">
            {{ t('cfgFleetResumeAction') }}
          </button>
        </div>

        <template v-if="activeRun">
          <p>
            <code>{{ activeRun.run_id }}</code> ·
            {{ t('cfgStatus') }}: <strong>{{ activeRun.state }}</strong> ·
            {{ t('cfgEpoch') }} {{ activeRun.data_epoch }}
          </p>
          <div v-if="activeRun.state === 'failed'" class="banner banner--warn" role="alert">
            <p>{{ t('cfgFleetStopBanner') }}</p>
            <p v-if="activeRun.error"><code>{{ activeRun.error.error }}</code>: {{ activeRun.error.detail }}</p>
          </div>
          <div v-if="hasRetryableTargets" class="actions">
            <button type="button" class="btn btn--primary" :disabled="busy" @click="onRetryRun">
              {{ t('cfgFleetRetryAction') }}
            </button>
          </div>

          <div v-for="target in activeRun.targets" :key="target.node_id" class="target-card">
            <div class="target-head">
              <span><code>{{ target.node_id }}</code> (#{{ target.ord + 1 }})</span>
              <Pill :tone="targetStateTone(target.state)" size="sm">{{ targetStateLabel(target.state) }}</Pill>
            </div>
            <p class="muted">
              <code>{{ target.application_id }}</code> · {{ target.base_url }} · {{ t('cfgRevision') }} {{ target.expected_revision }}
            </p>
            <div v-if="target.state === 'applied' || target.state === 'rolled_back'" class="actions">
              <button type="button" class="btn" :disabled="busy" @click="onRollbackPreview(target)">
                {{ t('cfgFleetRollbackPreview') }}
              </button>
            </div>
            <div v-if="rollbackStates[target.node_id]" class="subsection">
              <template v-if="!rollbackStates[target.node_id].committed">
                <p class="muted">
                  <code>{{ rollbackStates[target.node_id].preflight.plan_id }}</code> ·
                  {{ t('cfgRevision') }} {{ rollbackStates[target.node_id].preflight.expected_revision }}
                </p>
                <button type="button" class="btn btn--primary" :disabled="busy" @click="onRollbackCommit(target)">
                  {{ t('cfgFleetRollbackCommit') }}
                </button>
              </template>
              <p v-else role="status">
                {{ t('cfgFleetRollbackDone', { node: target.node_id, app: rollbackStates[target.node_id].rollback_application_id || '' }) }}
              </p>
            </div>
          </div>
        </template>
      </section>
    </template>
  </Panel>
</template>

<style scoped>
.card-section, .target-card, .subsection {
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}
.row, .actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
  align-items: flex-end;
}
.field {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  flex: 1 1 12rem;
}
.choose {
  display: flex;
  align-items: center;
  gap: var(--sp-1);
}
.target-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: var(--sp-2);
}
.banner--warn {
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--warn, #b45309);
  border-radius: var(--r-sm);
}
input:not([type="checkbox"]), textarea {
  width: 100%;
  box-sizing: border-box;
  color: var(--ink);
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  font: inherit;
  padding: var(--sp-2);
}
.muted { color: var(--muted); font-size: var(--fs-sm); }
.hash { word-break: break-all; }
.table-wrap { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; font-size: var(--fs-sm); }
th, td { text-align: left; border-bottom: 1px solid var(--line); padding: var(--sp-2); }
.btn { border: 1px solid var(--line); background: var(--surface); color: var(--ink); border-radius: var(--r-sm); padding: var(--sp-2); cursor: pointer; }
.btn--primary { background: var(--accent); color: var(--on-accent); }
.btn:disabled { opacity: .5; cursor: not-allowed; }
</style>
