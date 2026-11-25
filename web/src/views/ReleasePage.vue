<script setup lang="ts">
/**
 * Releases page (M8a): what is active, what was tried last, and the three
 * actions an operator needs — install a package, activate an installed one,
 * roll back. The device is the authority: every card renders facts from
 * `state.json` / the live core, never an assumption that a click worked.
 */
import { computed, onBeforeUnmount, ref } from 'vue'
import {
  activateRelease,
  getRelease,
  getReleaseLog,
  installRelease,
  isAbort,
  rollbackRelease,
  type ReleaseRef,
} from '../api'
import { t } from '../i18n'
import { useResource } from '../hooks/useResource'
import { usePoll } from '../hooks/usePoll'
import { toast } from '../hooks/useToast'
import { errorDetail, formatDateTime } from '../format'
import Panel from '../components/Panel.vue'
import AppIcon from '../components/AppIcon.vue'
import Pill from '../components/Pill.vue'
import LoadingBlock from '../components/LoadingBlock.vue'
import ErrorNotice from '../components/ErrorNotice.vue'
import EmptyState from '../components/EmptyState.vue'
import ConfirmDialog from '../components/ConfirmDialog.vue'

const view = useResource((signal) => getRelease({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => view.load(), 3000)

const logLines = ref<string[]>([])
const logPath = ref('')
const logError = ref<unknown>(null)
let logSeq = 0
let logController: AbortController | null = null

async function loadLog(): Promise<void> {
  const seq = ++logSeq
  logController?.abort()
  const controller = new AbortController()
  logController = controller
  try {
    const r = await getReleaseLog(200, { signal: controller.signal })
    if (seq !== logSeq) return
    logLines.value = r.lines
    logPath.value = r.path
    logError.value = null
  } catch (err) {
    if (seq !== logSeq || isAbort(err)) return
    logError.value = err
  }
}
usePoll(() => loadLog(), 5000)
onBeforeUnmount(() => {
  logSeq += 1
  logController?.abort()
  logController = null
})

const state = computed(() => view.data.value?.state ?? null)
const active = computed(() => state.value?.active ?? null)
const previous = computed(() => state.value?.previous ?? null)
const last = computed(() => state.value?.last ?? null)
const installer = computed(() => view.data.value?.installer ?? null)
const live = computed(() => view.data.value?.live ?? null)
const running = computed(() => {
  const s = last.value?.status
  return s === 'running' || s === 'rolling_back'
})

/** An install that succeeded but has not been activated is the one thing the
 *  "Activate" button is for; re-activating the current release is a no-op. */
const pendingActivation = computed<ReleaseRef | null>(() => {
  const run = last.value
  if (!run || run.kind !== 'install' || run.status !== 'ok' || !run.target) return null
  return run.target.id === active.value?.id ? null : run.target
})

/** What happened to the DB: restored on a schema-advancing rollback, kept
 *  after a failed attempt, or simply unchanged when the activation worked. */
const dbNote = computed(() => {
  const run = last.value
  if (!run?.db) return '—'
  if (run.db.restored) return t('releaseDbRestored')
  if (run.status === 'ok' || run.status === 'running') return t('releaseDbUnchanged')
  return t('releaseDbKept')
})

const buildMismatch = computed(() => {
  if (!active.value?.build_id || !live.value?.build.id) return false
  return active.value.build_id !== live.value.build.id
})

type Pending =
  | { kind: 'activate'; id: string }
  | { kind: 'install-activate' }
  | { kind: 'rollback' }
  | null
const pending = ref<Pending>(null)
const actionBusy = ref(false)

function statusLabel(status: string | undefined): string {
  switch (status) {
    case 'ok':
      return t('releaseStatusOk')
    case 'failed':
      return t('releaseStatusFailed')
    case 'rolled_back':
      return t('releaseStatusRolledBack')
    case 'rollback_failed':
      return t('releaseStatusRollbackFailed')
    case 'running':
      return t('releaseStatusRunning')
    case 'rolling_back':
      return t('releaseStatusRollingBack')
    default:
      return status ?? '—'
  }
}

function statusTone(status: string | undefined): 'ok' | 'danger' | 'warn' | 'info' {
  switch (status) {
    case 'ok':
      return 'ok'
    case 'rolled_back':
      return 'warn'
    case 'failed':
    case 'rollback_failed':
      return 'danger'
    default:
      return 'info'
  }
}

function kindLabel(kind: string | undefined): string {
  switch (kind) {
    case 'install':
      return t('releaseKindInstall')
    case 'activate':
      return t('releaseKindActivate')
    case 'rollback':
      return t('releaseKindRollback')
    case 'boot_guard':
      return t('releaseKindBootGuard')
    default:
      return kind ?? '—'
  }
}

function shortSha(sha: string | undefined): string {
  return sha ? sha.slice(0, 16) : '—'
}

async function refreshAll(): Promise<void> {
  await view.load()
  await loadLog()
}

async function runAction(fn: () => Promise<unknown>, done: string): Promise<void> {
  actionBusy.value = true
  try {
    await fn()
    toast.ok(done)
    await view.load()
  } catch (err) {
    toast.error(errorDetail(err))
  } finally {
    actionBusy.value = false
  }
}

async function confirmPending(): Promise<void> {
  const p = pending.value
  if (!p || actionBusy.value) return
  try {
    if (p.kind === 'install-activate') {
      await runAction(
        () => installRelease(confirmUrl.value, true),
        t('releaseTaskStarted', { task: t('releaseInstallActivate') }),
      )
    } else if (p.kind === 'activate') {
      await runAction(
        () => activateRelease(p.id, 'console'),
        t('releaseTaskStarted', { task: t('releaseActivate') }),
      )
    } else {
      await runAction(
        () => rollbackRelease('console rollback'),
        t('releaseTaskStarted', { task: t('releaseRollback') }),
      )
    }
  } finally {
    pending.value = null
  }
}

function cancelPending(): void {
  if (!actionBusy.value) pending.value = null
}

const installUrl = ref('')
const installError = ref('')
/** URL captured when the install+activate confirmation opened. */
const confirmUrl = ref('')

async function install(activate: boolean): Promise<void> {
  const url = installUrl.value.trim()
  if (!url) {
    installError.value = t('releaseInstallUrlRequired')
    return
  }
  installError.value = ''
  if (activate) {
    // Switching the running application restarts firmware + core; it already
    // asked for confirmation on the plain "activate" button, so the one-click
    // path asks the same question (audit 2026-11-14).
    confirmUrl.value = url
    pending.value = { kind: 'install-activate' }
    return
  }
  await runAction(
    () => installRelease(url, false),
    t('releaseTaskStarted', { task: t('releaseInstallOnly') }),
  )
}
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('releaseTitle') }}</h1>
        <p class="page__sub">{{ t('releaseSub') }}</p>
      </div>
    </header>

    <LoadingBlock v-if="view.loading.value && !view.data.value" :label="t('loading')" />
    <ErrorNotice v-else-if="view.error.value && !view.data.value" :error="view.error.value" @retry="view.load" />

    <div v-else class="page__grid">
      <ErrorNotice
        v-if="view.error.value && view.data.value"
        :error="view.error.value"
        :stale="view.stale.value"
        :at="view.updatedAt.value"
        compact
        @retry="view.load"
      />
      <Panel icon="package" :title="t('releaseCurrent')" tone="default">
        <template #actions>
          <Pill v-if="running" tone="info" size="sm">{{ t('releaseRunning') }}</Pill>
          <button type="button" class="btn" :disabled="view.loading.value" @click="refreshAll">
            <AppIcon name="reload" :size="13" /> {{ t('refresh') }}
          </button>
        </template>

        <div v-if="!active" class="empty-line">{{ t('releaseActiveNone') }}</div>
        <template v-else>
          <div class="facts">
            <div class="fact">
              <span class="fact__label">{{ t('releaseSerial') }}</span>
              <span class="mono">{{ active.serial }}</span>
            </div>
            <div class="fact">
              <span class="fact__label">{{ t('releaseCore') }}</span>
              <span class="mono">
                {{ active.version }} · {{ active.build_id || '—' }}
                <Pill v-if="active.build_id?.endsWith('-dirty')" tone="danger" size="sm">{{ t('releaseDirtyBuild') }}</Pill>
              </span>
            </div>
            <div class="fact">
              <span class="fact__label">{{ t('releaseFirmwareGateway') }}</span>
              <span class="mono">
                {{ active.firmware_version || '—' }} / {{ active.gateway_version || '—' }}
                <span v-if="active.firmware_build_id" class="muted small">· {{ active.firmware_build_id.slice(0, 12) }}</span>
              </span>
            </div>
            <div class="fact">
              <span class="fact__label">{{ t('releaseConsole') }}</span>
              <span class="mono">{{ active.console_index || '—' }}</span>
            </div>
            <div class="fact">
              <span class="fact__label">{{ t('releaseModel') }}</span>
              <span class="mono">{{ shortSha(active.model_sha256) }}</span>
            </div>
            <div class="fact">
              <span class="fact__label">{{ t('releaseActivatedAt') }}</span>
              <span>{{ formatDateTime(active.activated_ms) }}</span>
            </div>
            <div class="fact">
              <span class="fact__label">{{ t('releaseLive') }}</span>
              <span class="mono">
                {{ live?.build.id || '—' }} · schema {{ live?.schema ?? '—' }}
                <Pill v-if="buildMismatch" tone="warn" size="sm">{{ t('releaseBuildMismatch') }}</Pill>
              </span>
            </div>
          </div>

          <div class="row">
            <span class="muted">{{ t('releasePrevious') }}:</span>
            <span class="mono">{{ previous ? previous.id : t('none') }}</span>
            <Pill v-if="previous?.build_id?.endsWith('-dirty')" tone="danger" size="sm">
              {{ t('releaseDirtyBuild') }}
            </Pill>
            <button
              v-if="previous"
              type="button"
              class="btn btn--danger"
              :disabled="actionBusy || running"
              @click="pending = { kind: 'rollback' }"
            >
              <AppIcon name="back" :size="13" /> {{ t('releaseRollback') }}
            </button>
          </div>
        </template>
        <!-- Installed-but-not-active is shown even when nothing is active:
             the first install has no active release yet (audit 2026-11-14). -->
          <div v-if="pendingActivation" class="row">
            <span class="muted">{{ t('releaseInstalledNotActive') }}:</span>
            <span class="mono">{{ pendingActivation.id }}</span>
            <button
              type="button"
              class="btn"
              :disabled="actionBusy || running"
              @click="pending = { kind: 'activate', id: pendingActivation.id }"
            >
              <AppIcon name="play" :size="13" /> {{ t('releaseActivate') }}
            </button>
          </div>
      </Panel>

      <Panel icon="upload" :title="t('releaseInstall')" :hint="t('releaseInstallHint')" apply="restart">
        <div v-if="installer && !installer.present" class="warn-line">
          <AppIcon name="alert" :size="14" />
          {{ t('releaseInstallerMissing') }}
          <span class="mono muted">{{ installer.path }}</span>
        </div>
        <label class="field">
          <span class="field__label">{{ t('releaseInstallUrl') }}</span>
          <input
            v-model="installUrl"
            class="input mono"
            type="text"
            spellcheck="false"
            :placeholder="t('releaseInstallUrlPlaceholder')"
            :disabled="actionBusy || running"
          />
        </label>
        <p v-if="installError" class="error-line">{{ installError }}</p>
        <div class="row">
          <button
            type="button"
            class="btn"
            :disabled="actionBusy || running || (installer ? !installer.present : true)"
            @click="install(false)"
          >
            <AppIcon name="download" :size="13" /> {{ t('releaseInstallOnly') }}
          </button>
          <button
            type="button"
            class="btn btn--primary"
            :disabled="actionBusy || running || (installer ? !installer.present : true)"
            @click="install(true)"
          >
            <AppIcon name="upload" :size="13" /> {{ t('releaseInstallActivate') }}
          </button>
          <span class="muted small">{{ t('releaseAlwaysRestart') }}</span>
        </div>
      </Panel>

      <Panel icon="gauge" :title="t('releaseLast')">
        <div v-if="!last" class="empty-line">{{ t('releaseNoRun') }}</div>
        <template v-else>
          <div class="row">
            <Pill :tone="statusTone(last.status)" size="sm">{{ statusLabel(last.status) }}</Pill>
            <span class="muted">{{ kindLabel(last.kind) }}</span>
            <span class="mono muted">{{ last.target?.id || '—' }}</span>
            <span v-if="last.from" class="muted small">
              {{ t('releaseFrom') }} <span class="mono">{{ last.from.id }}</span>
            </span>
          </div>
          <div class="row muted small">
            <span>{{ formatDateTime(last.started_ms) }}</span>
            <span v-if="last.ended_ms">– {{ formatDateTime(last.ended_ms) }}</span>
          </div>
          <p v-if="last.error" class="error-line"><AppIcon name="alert" :size="13" /> {{ last.error }}</p>

          <p class="subhead">{{ t('releaseCriteria') }}</p>
          <table v-if="last.criteria.length" class="criteria">
            <tbody>
              <tr v-for="c in last.criteria" :key="c.name">
                <td class="criteria__mark" :class="c.ok ? 'ok' : 'fail'">
                  <AppIcon :name="c.ok ? 'check' : 'fail'" :size="13" />
                </td>
                <td class="mono criteria__name">{{ c.name }}</td>
                <td class="muted">{{ c.detail }}</td>
              </tr>
            </tbody>
          </table>
          <EmptyState v-else :title="t('empty')" />

          <template v-if="last.db">
            <p class="subhead">{{ t('releaseDb') }}</p>
            <div class="facts">
              <div class="fact">
                <span class="fact__label">{{ t('releaseDbSnapshot') }}</span>
                <span class="mono">{{ last.db.snapshot || '—' }}</span>
              </div>
              <div class="fact">
                <span class="fact__label">{{ t('releaseDbSchema') }}</span>
                <span class="mono">{{ last.db.schema_before ?? '—' }} – {{ last.db.schema_after ?? '—' }}</span>
              </div>
              <div class="fact">
                <span class="fact__label">{{ t('releaseHealth') }}</span>
                <span>{{ dbNote }}</span>
              </div>
            </div>
          </template>
        </template>
      </Panel>

      <Panel icon="terminal" :title="t('releaseLog')" :hint="t('releaseLogHint')">
        <template #actions>
          <button type="button" class="btn" @click="loadLog">
            <AppIcon name="reload" :size="13" /> {{ t('releaseRefreshLog') }}
          </button>
        </template>
        <p v-if="logPath" class="mono muted small">{{ logPath }}</p>
        <ErrorNotice v-if="logError" :error="logError" @retry="loadLog" />
        <pre v-if="logLines.length" class="log mono">{{ logLines.join('\n') }}</pre>
        <EmptyState v-else :title="t('empty')" />
      </Panel>
    </div>

    <ConfirmDialog
      v-if="pending"
      :title="
        pending.kind === 'rollback'
          ? t('releaseRollbackTitle')
          : pending.kind === 'install-activate'
            ? t('releaseInstallActivateTitle')
            : t('releaseActivateTitle')
      "
      :message="
        pending.kind === 'rollback'
          ? t('releaseRollbackMessage')
          : pending.kind === 'install-activate'
            ? t('releaseInstallActivateMessage')
            : t('releaseActivateMessage')
      "
      :confirm-label="
        pending.kind === 'rollback'
          ? t('releaseRollback')
          : pending.kind === 'install-activate'
            ? t('releaseInstallActivate')
            : t('releaseActivate')
      "
      :cancel-label="t('cancel')"
      :tone="pending.kind === 'rollback' ? 'danger' : 'default'"
      :busy="actionBusy"
      :busy-label="t('releaseRunning')"
      @confirm="confirmPending"
      @cancel="cancelPending"
    />
  </section>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}
.page__title {
  font-size: var(--fs-xl);
}
.page__sub {
  margin-top: var(--sp-1);
  color: var(--muted);
  font-size: var(--fs-sm);
  max-width: 75ch;
}
.page__grid {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}
.facts {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(210px, 1fr));
  gap: var(--sp-2) var(--sp-4);
}
.fact {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.fact__label {
  color: var(--muted);
  font-size: var(--fs-xs);
}
.row {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}
.field {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
}
.field__label {
  color: var(--muted);
  font-size: var(--fs-sm);
}
.input {
  width: 100%;
  padding: 8px 10px;
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
  color: var(--ink);
}
.subhead {
  margin-top: var(--sp-2);
  color: var(--muted);
  font-size: var(--fs-xs);
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
.criteria {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-sm);
}
.criteria td {
  padding: 3px 6px;
  vertical-align: top;
  border-bottom: 1px solid var(--line);
}
.criteria__name {
  white-space: nowrap;
  width: 1%;
}
.criteria__mark.ok {
  color: var(--ok);
}
.criteria__mark.fail {
  color: var(--danger);
}
.log {
  max-height: 320px;
  overflow: auto;
  background: var(--surface-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  padding: var(--sp-2);
  font-size: var(--fs-xs);
  white-space: pre-wrap;
  word-break: break-word;
}
.error-line {
  display: flex;
  align-items: center;
  gap: var(--sp-1);
  color: var(--danger);
  font-size: var(--fs-sm);
}
.warn-line {
  display: flex;
  align-items: center;
  gap: var(--sp-1);
  color: var(--warn);
  font-size: var(--fs-sm);
  flex-wrap: wrap;
}
.empty-line {
  color: var(--muted);
  font-size: var(--fs-sm);
}
.small {
  font-size: var(--fs-xs);
}
.muted {
  color: var(--muted);
}
.mono {
  font-family: var(--font-mono);
}
</style>
