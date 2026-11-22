/**
 * The Chinese dictionary: the per-page files in this directory merged into one
 * object.
 *
 * `Key` is derived from THIS object, so a new string is declared in exactly one
 * place — the file for the page that shows it — and the English files are typed
 * `Record<Key, string>`, which makes a missing or extra translation a compile
 * error (RI-6). Each file keeps its own page's keys; merging never renames one.
 */
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

export const zh = {
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
} as const

/** Every key the console can ask `t()` for (derived from the Chinese copy). */
export type Key = keyof typeof zh
