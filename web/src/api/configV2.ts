/** Initial configuration-v2 surface; unsupported blocks remain explicit. */
import { request, type RequestOptions } from './client'
export type ConfigValue = null | boolean | number | string | ConfigValue[] | { [key: string]: ConfigValue }
export type ConfigPurpose = 'family_policy' | 'node_config' | 'same_node_restore'
export interface ConfigBounds {
  min?: number
  max?: number
  max_len?: number
  max_items?: number
  allowed?: string[]
}
export interface ConfigField {
  path: string
  kind: 'string' | 'boolean' | 'object' | 'array' | 'number' | 'integer' | 'json'
  nullable: boolean
  array_item_kind: string | null
  scope: 'family_or_node' | 'node' | 'unit' | 'data' | 'runtime' | 'boot' | 'secret'
  supported: boolean
  purposes: string[]
  unit: string
  bounds: ConfigBounds | null
  default_source: string
  validator: string
  secret_policy: string
  apply: string
  rollback: string
  clear_behavior: string
}
export interface ConfigApplication {
  application_id: string
  plan_id: string
  plan_hash: string
  config_revision: number
  data_epoch: number
  purpose: ConfigPurpose
  target: { household_id: string; node_id: string; enrolled: boolean }
  actor: string
  change_reason: string
  created_ms: number
  state: 'saved_pending' | 'unchanged'
  blocks: { name: string; state: 'saved_pending' | 'unchanged' }[]
  diff: ConfigPlan['diff']
  layer_mode: string
}
export interface ConfigState {
  config_revision: number
  data_epoch: number
  core_version: string
  schema_version: number
  target: { household_id: string; node_id: string; enrolled: boolean; node_id_source: string }
  layer_mode: string
  fields: ConfigField[]
  stored: Record<string, ConfigValue>
  resolved: Record<string, ConfigValue>
  origins: Record<string, string>
  bindings: { binding_id: string; bound: boolean; scope: 'stored_target_binding_only'; environment_binding: 'not_probed' }[]
  capabilities: { supported_blocks: string[]; rollback: boolean; fleet: boolean; enrollment: boolean; can_enroll: boolean; weather_reviewable?: boolean; unavailable_blocks?: string[] }
  applications: ConfigApplication[]
}
export interface ConfigOperation { action: 'set' | 'replace' | 'clear' | 'preserve'; path: string; value?: ConfigValue }
export interface ConfigPackage {
  format: 'home-mate-config'
  version: 2
  package_id: string
  package_revision: number
  created_ms: number
  purpose: ConfigPurpose
  source: { node_id: string; config_revision: number; core_version: string; schema_version: number }
  target: { household_id: string; node_id?: string }
  compatibility: { core_api_schema: number; gateway_protocol_min: number; gateway_protocol_max: number; required_capabilities: string[] }
  change_reason: string
  blocks: { name: string; schema_version: number; operations: ConfigOperation[] }[]
  bindings: { action: 'preserve' | 'require_binding'; binding_id: string }[]
}
export interface ConfigPlan {
  plan_id: string
  plan_hash: string
  expected_revision: number
  expires_ms: number
  diff: { path: string; before: ConfigValue; after: ConfigValue; origin: string; action: string; layer_before: ConfigValue; layer_after: ConfigValue; layer_before_null?: boolean; layer_after_null?: boolean }[]
  migration_report: ConfigValue
  blocked: ConfigValue[]
}
export interface ConfigCommit { plan_id: string; plan_hash: string; expected_revision: number; application_id: string }
export const getConfigState = (opts?: RequestOptions) => request<ConfigState>('/api/v1/config/state', {}, opts)
export const enrollConfigNode = () => request<{ household_id: string; node_id: string; enrolled: boolean }>('/api/v1/config/enrollment', { method: 'PUT', body: '{}' })
export const preflightConfig = (pkg: ConfigPackage) => request<ConfigPlan>('/api/v1/config/preflight', { method: 'POST', body: JSON.stringify({ package: pkg }) })
export const commitConfig = (body: ConfigCommit) => request<ConfigApplication>('/api/v1/config/applications', { method: 'POST', body: JSON.stringify(body) })
export const getConfigApplication = (id: string, opts?: RequestOptions) => request<ConfigApplication>(`/api/v1/config/applications/${encodeURIComponent(id)}`, {}, opts)

/** LAN HTTP lacks randomUUID on some browsers; getRandomValues remains available. */
export function newConfigIdentity(): string {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID()
  const bytes = crypto.getRandomValues(new Uint8Array(16))
  bytes[6] = (bytes[6] & 0x0f) | 0x40
  bytes[8] = (bytes[8] & 0x3f) | 0x80
  const hex = [...bytes].map(value => value.toString(16).padStart(2, '0')).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}

export interface ConfigPublication { block: 'persona' | 'services' | 'weather' | 'home' | 'music' | 'news' | 'search' | 'openapi' | 'mode' | 'node' | 'device_led_configs'; application_id: string; revision: number; data_epoch: number; state: string; error: string | null; isolation: 'desired_until_worker' | 'legacy_projection' }
export interface ConfigReadback {
  initial_receipt: ConfigApplication
  observed_revision: number
  data_epoch: number
  checked_ms: number
  state: 'partial_readback' | 'superseded'
  fields: { path: string; state: string; value: ConfigValue; dependency: string | null }[]
  remote_health: 'not_probed'
  publication_isolation: 'not_implemented' | 'persona' | 'core_blocks'
  publication?: ConfigPublication | null
  publications?: ConfigPublication[]
}
export const getConfigReadback = (id: string, opts?: RequestOptions) => request<ConfigReadback>(`/api/v1/config/applications/${encodeURIComponent(id)}/readback`, {}, opts)
export const previewConfigRollback = (id: string, expected_revision: number) => request<ConfigPlan>(`/api/v1/config/applications/${encodeURIComponent(id)}/rollback-preflight`, { method: 'POST', body: JSON.stringify({ expected_revision }) })

export interface Dot2dPersonaProfile {
  profile_id: string
  filename: string
  scope: 'persona_only'
  complete_luna_profile: false
  target_assurance: { enrolled_core_node_id: 'dot2-d'; physical_device_identity: 'not_verified' }
  package: ConfigPackage & { content_hash: string }
  preflight: ConfigPlan
  preserved: string[]
  wake_policy: { action: 'preserve'; recommended_default: string; gateway_effective_state: 'not_verified' }
  unavailable: { issue: number; paths: string[] }[]
}
export const generateDot2dPersonaProfile = () => request<Dot2dPersonaProfile>('/api/v1/config/profiles/dot2-d-persona', { method: 'POST', body: '{}' })

export interface ConfigExportRequest {
  format?: 'v2' | 'v1'
  purpose?: ConfigPurpose
  blocks?: string[]
}
export interface ConfigExportResponse {
  format_version: 1 | 2
  filename: string
  redacted: boolean
  excluded: string[]
  package?: ConfigPackage & { content_hash?: string }
  document?: ConfigValue
  loss_report?: { domain: string; status: string; detail: string }[]
}
export const exportConfig = (body: ConfigExportRequest = {}) =>
  request<ConfigExportResponse>('/api/v1/config/exports', { method: 'POST', body: JSON.stringify(body) })

export interface Dot2dFamilyBlocker {
  path: string
  reason: string
  detail: string
}
export interface Dot2dFamilyProfile {
  profile_id: string
  filename: string
  scope: 'dot2d_family'
  complete_luna_profile: true
  target_assurance: {
    enrolled_core_node_id: 'dot2-d'
    immutable_target_lock: 'dot2-d'
    physical_device_identity: 'not_verified'
  }
  package: ConfigPackage & { content_hash: string }
  preflight: ConfigPlan
  retained_unit_refs: {
    'unit_acoustic_profiles.dot2-d': Record<string, ConfigValue>
    audio_eq: Record<string, ConfigValue>
    services: Record<string, ConfigValue>
    bindings: ConfigState['bindings']
  }
  compatibility: {
    core_api_schema: number
    required_capabilities: string[]
    readback_requirements: {
      required_blocks: string[]
      publication_state: string
    }
  }
  rollback_instructions: {
    preflight_endpoint: string
    commit_endpoint: string
    expected_revision: number
  }
  blockers: Dot2dFamilyBlocker[]
}
export const generateDot2dFamilyProfile = () =>
  request<Dot2dFamilyProfile>('/api/v1/config/profiles/dot2-d-family', { method: 'POST', body: '{}' })

export interface HouseholdInvitationRequest {
  expected_node_id: string
  ttl_ms?: number
}
export interface HouseholdInvitation {
  invitation_id: string
  household_id: string
  expected_node_id: string
  token: string
  membership_generation: number
  data_epoch: number
  created_ms: number
  expires_ms: number
}
export interface RedeemInvitationRequest {
  token: string
  node_id: string
}
export interface RedeemInvitationResponse {
  invitation_id: string
  household_id: string
  expected_node_id: string
  membership_generation: number
}
export interface JoinHouseholdRequest {
  token: string
  issuer_node_id?: string
  issuer_base_url?: string
  issuer_token?: string
}
export interface JoinHouseholdResponse {
  household_id: string
  node_id: string
  enrolled: boolean
  invitation_id: string
  membership_generation: number
}
export const issueHouseholdInvitation = (body: HouseholdInvitationRequest) =>
  request<HouseholdInvitation>('/api/v1/config/household/invitations', { method: 'POST', body: JSON.stringify(body) })
export const redeemHouseholdInvitation = (body: RedeemInvitationRequest) =>
  request<RedeemInvitationResponse>('/api/v1/config/household/invitations/redeem', { method: 'POST', body: JSON.stringify(body) })
export const joinHousehold = (body: JoinHouseholdRequest) =>
  request<JoinHouseholdResponse>('/api/v1/config/household/join', { method: 'POST', body: JSON.stringify(body) })

export interface FleetPreflightRequest {
  package: ConfigPackage | Record<string, ConfigValue>
  targets?: string[]
  sanitize_family?: boolean
}
export interface FleetPreflightTarget {
  node_id: string
  ok: boolean
  plan_id?: string
  plan_hash?: string
  expected_revision?: number
  diff?: ConfigPlan['diff']
  blocked?: ConfigValue[]
  error?: { code: string; detail: string; pointer?: string | null }
}
export interface FleetPreflightResponse {
  household_id: string
  ordered_targets: string[]
  all_ok: boolean
  migration_report: ConfigValue
  package: ConfigPackage & { content_hash?: string }
  targets: FleetPreflightTarget[]
}
export interface FleetTargetPlan {
  node_id: string
  plan_id: string
  plan_hash: string
  expected_revision: number
}
export interface FleetRunRequest {
  run_id?: string
  package: ConfigPackage | Record<string, ConfigValue>
  targets?: string[]
  sanitize_family?: boolean
  plans?: FleetTargetPlan[]
}
export type FleetTargetState = 'pending' | 'committing' | 'applied' | 'failed' | 'skipped' | 'rolled_back'
export interface FleetRunError {
  error: string
  detail: string
  pointer?: string | null
  status?: number
}
export interface FleetRunTarget {
  node_id: string
  ord: number
  base_url: string
  plan_id: string
  plan_hash: string
  expected_revision: number
  application_id: string
  state: FleetTargetState
  result: ConfigValue
  updated_ms: number
}
export interface FleetRunStatus {
  run_id: string
  household_id: string
  data_epoch: number
  package_hash: string
  state: 'running' | 'applied' | 'failed'
  created_ms: number
  updated_ms: number
  error: FleetRunError | null
  targets: FleetRunTarget[]
}
export interface FleetRollbackRequest {
  expected_revision?: number
  commit?: boolean
}
export interface FleetRollbackResponse {
  run_id: string
  node_id: string
  source_application_id: string
  rollback_application_id?: string
  committed: boolean
  preflight: ConfigPlan
  commit?: ConfigApplication
  readback?: ConfigReadback
}
export const preflightFleet = (body: FleetPreflightRequest) =>
  request<FleetPreflightResponse>('/api/v1/config/fleet/preflight', { method: 'POST', body: JSON.stringify(body) })
export const commitFleet = (body: FleetRunRequest) =>
  request<FleetRunStatus>('/api/v1/config/fleet/runs', { method: 'POST', body: JSON.stringify(body) })
export const getFleetRun = (id: string, opts?: RequestOptions) =>
  request<FleetRunStatus>(`/api/v1/config/fleet/runs/${encodeURIComponent(id)}`, {}, opts)
export const retryFleetRun = (id: string) =>
  request<FleetRunStatus>(`/api/v1/config/fleet/runs/${encodeURIComponent(id)}/retry`, { method: 'POST', body: '{}' })
export const rollbackFleetTarget = (id: string, node: string, body: FleetRollbackRequest = {}) =>
  request<FleetRollbackResponse>(`/api/v1/config/fleet/runs/${encodeURIComponent(id)}/targets/${encodeURIComponent(node)}/rollback-preflight`, { method: 'POST', body: JSON.stringify(body) })
