/**
 * Submit state for one card: busy, one message, one tone.
 *
 * Every card uses the same shape so a save always ends in the same three
 * visible outcomes — saved, failed with the server's detail, or still busy —
 * instead of each card inventing its own "✓".
 */
import { computed, ref } from 'vue'
import { t } from '../i18n'
import { errorDetail } from '../format'

export type SaveTone = '' | 'ok' | 'error'

export function useSave() {
  const busy = ref(false)
  const message = ref('')
  const tone = ref<SaveTone>('')

  async function run(action: () => Promise<string | void>): Promise<boolean> {
    if (busy.value) return false
    busy.value = true
    message.value = ''
    tone.value = ''
    try {
      const detail = await action()
      message.value = detail || t('saved')
      tone.value = 'ok'
      return true
    } catch (err) {
      message.value = errorDetail(err) || t('saveFailed')
      tone.value = 'error'
      return false
    } finally {
      busy.value = false
    }
  }

  function clear(): void {
    message.value = ''
    tone.value = ''
  }

  return {
    busy,
    message,
    tone,
    run,
    clear,
    ok: computed(() => tone.value === 'ok'),
  }
}
