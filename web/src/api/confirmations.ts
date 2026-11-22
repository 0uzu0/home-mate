import { request, type RequestOptions } from './client'

export type ConfirmationState = 'pending' | 'approved' | 'consumed' | 'rejected' | 'expired' | 'cancelled' | 'stale'
export type ToolStatus = 'completed' | 'failed' | 'partial' | 'cancelled' | 'unknown' | 'awaiting_confirmation'
export type TaskStatus = 'running' | 'awaiting_confirmation' | 'completed' | 'failed' | 'cancelled'
export interface ConfirmationOutcome {
  status: ToolStatus | 'running'
  evidence: 'accepted' | 'submitted' | 'sent' | 'observed' | 'device_confirmed' | 'unknown'
  result: unknown
  error?: { code: string; retryable: boolean; detail: string }
  fact?: { subject: string; action: string; confirmed: boolean; value?: string; detail?: string }
}
export interface ConfirmationAction {
  id: string
  revision: number
  state: ConfirmationState
  task_id: string
  call_id: string
  session_id: string
  created_ms: number
  expires_ms: number
  ttl_ms: number
  preview: {
    tool: 'openapi.call'
    operation: string
    service: string
    method: string
    target: string
    parameters: Record<string, unknown>
    scope: { person_id?: string; device_id?: string; room?: string }
  }
  requester: { kind: 'management' | 'physical'; device_id?: string }
  approver_policy: 'requester' | 'paired_management'
  approved_ms: number | null
  consumed_ms: number | null
  outcome: ConfirmationOutcome | null
}
export interface ConfirmationList { actions: ConfirmationAction[]; next_cursor: string | null }
export type ConfirmationTransition = 'approve' | 'reject' | 'cancel' | 'execute'
export const listConfirmations = (
  state?: ConfirmationState[], cursor?: string | null, opts?: RequestOptions,
) => {
  const query = new URLSearchParams({ limit: '64' })
  if (state?.length) query.set('state', state.join(','))
  if (cursor) query.set('cursor', cursor)
  return request<ConfirmationList>(`/api/v1/confirmations?${query}`, {}, opts)
}
export const getConfirmation = (id: string, opts?: RequestOptions) =>
  request<ConfirmationAction>(`/api/v1/confirmations/${encodeURIComponent(id)}`, {}, opts)

/** No operation payload or client-created authority can enter a transition. */
export function transitionConfirmation(action: ConfirmationAction, transition: ConfirmationTransition, opts?: RequestOptions) {
  return request<ConfirmationAction>(`/api/v1/confirmations/${encodeURIComponent(action.id)}/${transition}`, {
    method: 'POST',
    body: JSON.stringify({ expected_state: action.state, expected_revision: action.revision }),
  }, opts)
}
