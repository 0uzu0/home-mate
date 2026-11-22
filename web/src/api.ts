/**
 * Typed API client facade: re-exports domain modules under `./api/*` so every
 * file in `web/src/` stays well below the 1,400-line ceiling while preserving
 * all existing `from '../api'` imports.
 */

export * from './api/client'
export * from './api/devices'
export * from './api/services'
export * from './api/diagnostics'
export * from './api/confirmations'
