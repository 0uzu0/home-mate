<script setup lang="ts">
/** Music: two independent sources, write-only secrets, a real probe. */
import { ref, watch } from 'vue'
import {
  getMusic,
  putMusic,
  testMusic,
  type MusicProbe,
  type MusicStatus,
} from '../../api'
import { t } from '../../i18n'
import { errorDetail } from '../../format'
import { useSave } from '../../hooks/useSave'
import { useResource } from '../../hooks/useResource'
import { usePoll } from '../../hooks/usePoll'
import Panel from '../Panel.vue'
import ErrorNotice from '../ErrorNotice.vue'
import AppIcon from '../AppIcon.vue'
import Pill from '../Pill.vue'
import Toggle from '../Toggle.vue'
import TextInput from '../TextInput.vue'
import PasswordField from '../PasswordField.vue'

const music = useResource((signal) => getMusic({ signal }), { timeoutMs: 6000, retries: 1 })
usePoll(() => music.load(), 15000)

interface Draft {
  enabled: boolean
  prefer_local: boolean
  libraryEndpoint: string
  libraryUser: string
  librarySecret: string
  onlineEndpoint: string
  onlineUser: string
  onlineSecret: string
  revision: string
}

const draft = ref<Draft | null>(null)
const probe = ref<MusicProbe | null>(null)
const probing = ref(false)
const probeError = ref('')
const { busy, message, tone, run } = useSave()

function fromStatus(status: MusicStatus): Draft {
  return {
    enabled: status.enabled,
    prefer_local: status.prefer_local,
    libraryEndpoint: status.library?.endpoint ?? '',
    libraryUser: status.library?.user ?? '',
    librarySecret: '',
    onlineEndpoint: status.online?.endpoint ?? '',
    onlineUser: status.online?.user ?? '',
    onlineSecret: '',
    revision: status.revision,
  }
}

watch(
  () => music.data.value,
  (status) => {
    if (status && draft.value === null) draft.value = fromStatus(status)
  },
  { immediate: true },
)

async function save(): Promise<void> {
  if (!draft.value) return
  const current = draft.value
  await run(async () => {
    const status = await putMusic(
      {
        enabled: current.enabled,
        prefer_local: current.prefer_local,
        library: current.libraryEndpoint
          ? {
              endpoint: current.libraryEndpoint.trim(),
              user: current.libraryUser.trim(),
              ...(current.librarySecret ? { secret: current.librarySecret } : {}),
            }
          : undefined,
        online: current.onlineEndpoint
          ? {
              endpoint: current.onlineEndpoint.trim(),
              user: current.onlineUser,
              ...(current.onlineSecret ? { secret: current.onlineSecret } : {}),
            }
          : undefined,
        revision: current.revision || undefined,
      },
      { timeoutMs: 10000 },
    )
    // The server returns the NEW revision; keeping the old one would make the
    // next save a 409.
    draft.value = { ...fromStatus(status), librarySecret: '', onlineSecret: '' }
    await music.load()
  })
}

async function runProbe(): Promise<void> {
  probing.value = true
  probe.value = null
  probeError.value = ''
  try {
    probe.value = await testMusic()
  } catch (err) {
    probeError.value = errorDetail(err)
  } finally {
    probing.value = false
  }
}

function reload(): void {
  void music.load().then(() => {
    if (music.data.value) draft.value = fromStatus(music.data.value)
  })
}
</script>

<template>
  <Panel
    icon="music" apply="immediate" :title="t('musicTitle')" :hint="t('musicHint')" :tone="music.data.value?.enabled ? 'default' : 'warn'">
    <template #actions>
      <Pill :tone="music.data.value?.enabled ? 'ok' : 'warn'" size="sm">
        {{ music.data.value?.enabled ? t('configured') : t('unconfigured') }}
      </Pill>
    </template>

    <ErrorNotice
      v-if="music.error.value"
      :error="music.error.value"
      compact
      @retry="music.load()"
    />
    <div v-if="music.loading.value && !draft" class="muted">{{ t('loading') }}</div>

    <template v-if="draft">
      <div class="music__switches">
        <Toggle v-model="draft.enabled" :label="t('musicEnabled')" />
        <Toggle v-model="draft.prefer_local" :label="t('musicPreferLocal')" />
      </div>

      <div class="music__sources">
        <div class="music__source">
          <p class="music__source-title">{{ t('musicLibrary') }}</p>
          <TextInput v-model="draft.libraryEndpoint" :label="t('musicEndpoint')" wide placeholder="http://192.168.1.x:4533" />
          <TextInput v-model="draft.libraryUser" :label="t('musicUser')" />
          <PasswordField
            v-model="draft.librarySecret"
            :label="t('musicLibrarySecret')"
            :is-set="music.data.value?.library?.secret_set ?? false"
            :placeholder="music.data.value?.library?.secret_set ? t('musicSecretSet') : t('musicSecretUnset')"
          />
        </div>
        <div class="music__source">
          <p class="music__source-title">{{ t('musicOnline') }}</p>
          <TextInput v-model="draft.onlineEndpoint" :label="t('musicEndpoint')" wide placeholder="http://192.168.1.x:PORT" />
          <TextInput v-model="draft.onlineUser" :label="t('musicOnlineUser')" wide />
          <PasswordField
            v-model="draft.onlineSecret"
            :label="t('musicOnlineSecret')"
            :is-set="music.data.value?.online?.secret_set ?? false"
            :placeholder="music.data.value?.online?.secret_set ? t('musicSecretSet') : t('musicSecretUnset')"
          />
        </div>
      </div>

      <div class="music__actions">
        <button type="button" class="btn btn--primary" :disabled="busy" @click="save">
          <AppIcon name="save" :size="13" /> {{ busy ? t('saving') : t('save') }}
        </button>
        <button type="button" class="btn" :disabled="probing" @click="runProbe">
          <AppIcon name="probes" :size="13" /> {{ probing ? t('testing') : t('musicTest') }}
        </button>
        <button type="button" class="btn" @click="reload"><AppIcon name="reload" :size="13" /> {{ t('reload') }}</button>
        <span v-if="message" class="music__msg" :class="`music__msg--${tone}`">{{ message }}</span>
      </div>

      <div v-if="probe" class="music__probe">
        <Pill :tone="probe.ok ? 'ok' : 'danger'" size="sm" mono>
          {{
            probe.ok
              ? t('musicTestOk', {
                  lc: probe.library.count,
                  lms: probe.library.latency_ms,
                  oc: probe.online.count,
                  oms: probe.online.latency_ms,
                })
              : t('musicTestFail', {
                  detail:
                    probe.library.configured && !probe.library.ok
                      ? probe.library.detail
                      : probe.online.detail,
                })
          }}
        </Pill>
      </div>
      <p v-if="probeError" class="music__msg music__msg--error">{{ probeError }}</p>
    </template>
  </Panel>
</template>

<style scoped>
.music__switches {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.music__sources {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr));
  gap: var(--sp-4);
  align-items: start;
}

.music__source {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-3);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  background: var(--surface-2);
}

.music__source-title {
  font-size: var(--fs-sm);
  font-weight: 650;
  color: var(--ink-2);
}

.music__actions {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.btn {
  border: 1px solid var(--line-strong);
  background: var(--surface-2);
  color: var(--ink);
  border-radius: var(--r-sm);
  padding: 0.24rem var(--sp-3);
  font-size: var(--fs-sm);
  cursor: pointer;
}

.btn--primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.music__msg--ok {
  color: var(--ok);
  font-size: var(--fs-sm);
}

.music__msg--error {
  color: var(--danger);
  font-size: var(--fs-sm);
}

.muted {
  color: var(--muted);
  font-size: var(--fs-sm);
}
</style>
