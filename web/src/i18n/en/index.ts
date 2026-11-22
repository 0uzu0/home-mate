/**
 * The English dictionary, typed as `Record<Key, string>`: `Key` comes from the
 * Chinese side, so this file fails to compile when a translation is missing or
 * when a key exists here but not in `zh/`. The copy is unchanged from the
 * single-file dictionary it was split out of (issue #31).
 */
import type { Key } from '../zh'
import shell from './shell'
import status from './status'
import services from './services'
import interaction from './interaction'
import devices from './devices'
import diagnostics from './diagnostics'
import settings from './settings'
import setup from './setup'
import config from './config'
import confirmations from './confirmations'

const en: Record<Key, string> = {
  ...shell,
  ...status,
  ...services,
  ...interaction,
  ...devices,
  ...diagnostics,
  ...settings,
  ...setup,
  ...config,
  ...confirmations,
}

export default en
