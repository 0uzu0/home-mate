import { request, type RequestOptions, type ServiceName, type ServiceStatus } from './client'

export interface ServicePatch {
  provider: string
  base_url?: string
  model?: string
  api_key_env?: string
  voice?: string
  thinking?: string
  api_key?: string
}

export const getServices = (opts?: RequestOptions) =>
  request<Record<ServiceName, ServiceStatus>>('/api/v0/config/services', {}, opts)

export const putService = (service: ServiceName, patch: ServicePatch, opts?: RequestOptions) =>
  request<{ state: string; service: ServiceStatus }>(
    `/api/v0/config/services/${service}`,
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export interface ServiceProbe {
  ok: boolean
  reply?: string
  text?: string
  latency_ms?: number
  audio_ms?: number
  bytes?: number
  provider?: string
  voice?: string
  error?: string
}

export const testService = (service: ServiceName, opts?: RequestOptions) =>
  request<ServiceProbe>(
    `/api/v0/config/services/${service}/test`,
    { method: 'POST' },
    { timeoutMs: 30000, ...opts },
  )

// ── Music ───────────────────────────────────────────────────────────────

export interface MusicConnectionStatus {
  endpoint: string
  user: string
  secret_set: boolean
}

export interface MusicStatus {
  enabled: boolean
  prefer_local: boolean
  library: MusicConnectionStatus | null
  online: MusicConnectionStatus | null
  revision: string
}

export interface MusicProbeSource {
  configured: boolean
  ok: boolean
  latency_ms: number
  count: number
  detail: string
}

export interface MusicProbe {
  ok: boolean
  enabled: boolean
  configured_sources: number
  library: MusicProbeSource
  online: MusicProbeSource
}

export interface MusicPatch {
  enabled: boolean
  prefer_local: boolean
  library?: { endpoint: string; user: string; secret?: string }
  online?: { endpoint: string; user?: string; secret?: string }
  revision?: string
}

export const getMusic = (opts?: RequestOptions) =>
  request<MusicStatus>('/api/v0/config/services/music', {}, opts)

export const putMusic = (patch: MusicPatch, opts?: RequestOptions) =>
  request<MusicStatus>(
    '/api/v0/config/services/music',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testMusic = (opts?: RequestOptions) =>
  request<MusicProbe>(
    '/api/v0/config/services/music/test',
    { method: 'POST' },
    { timeoutMs: 20000, ...opts },
  )

// ── Weather ─────────────────────────────────────────────────────────────

export interface WeatherPlace {
  alias: string
  name: string
  latitude: number
  longitude: number
  timezone: string
  city_code: string
}

export interface WeatherStatus {
  enabled: boolean
  provider: string
  endpoint: string
  api_key_env: string
  api_key_set: boolean
  places: WeatherPlace[]
  default_alias: string
  max_days: number
  state: string
  revision: string
}

export interface WeatherProbe {
  configured: boolean
  enabled: boolean
  state: string
  provider: string
  places: number
  ok: boolean
  place?: string
  place_name?: string
  latency_ms?: number
  temperature_c?: number
  condition?: string
  observed_at?: string
  detail?: string
  error?: string
}

export interface GeocodePlace {
  name: string
  admin: string
  country: string
  label: string
  latitude: number
  longitude: number
  timezone: string
  city_code?: string
}

export interface WeatherPatch {
  enabled: boolean
  provider?: string
  endpoint?: string
  api_key_env?: string
  places: WeatherPlace[]
  default_alias?: string
  max_days?: number
  api_key?: string
  revision?: string
}

export const getWeather = (opts?: RequestOptions) =>
  request<WeatherStatus>('/api/v0/config/services/weather', {}, opts)

export const putWeather = (patch: WeatherPatch, opts?: RequestOptions) =>
  request<WeatherStatus>(
    '/api/v0/config/services/weather',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testWeather = (opts?: RequestOptions) =>
  request<WeatherProbe>(
    '/api/v0/config/services/weather/test',
    { method: 'POST' },
    { timeoutMs: 20000, ...opts },
  )

export const geocodePlace = (query: string, opts?: RequestOptions) =>
  request<{ results: GeocodePlace[]; query: string; error?: string; detail?: string }>(
    `/api/v0/config/services/weather/geocode?q=${encodeURIComponent(query)}`,
    {},
    { timeoutMs: 15000, ...opts },
  )

// ── News ────────────────────────────────────────────────────────────────

export type NewsKind = 'rss' | 'atom' | 'json'

export interface NewsSource {
  alias: string
  url: string
  kind: NewsKind
  max_items: number
}

export interface NewsStatus {
  enabled: boolean
  sources: NewsSource[]
  default_alias: string
  timeout_ms: number
  max_sources?: number
  state: string
  revision: string
}

export interface NewsSourceProbe {
  alias: string
  kind: string
  state: string
  items?: number
  latency_ms?: number
  code?: string
  detail?: string
}

export interface NewsProbe {
  ok: boolean
  state: string
  detail?: string
  sources: NewsSourceProbe[]
  http_requests: number
  latency_ms: number
}

export interface NewsPatch {
  enabled?: boolean
  sources?: NewsSource[]
  default_alias?: string
  timeout_ms?: number
  max_sources?: number
  revision?: string
}

export const getNews = (opts?: RequestOptions) =>
  request<NewsStatus>('/api/v0/config/services/news', {}, opts)

export const putNews = (patch: NewsPatch, opts?: RequestOptions) =>
  request<NewsStatus>(
    '/api/v0/config/services/news',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testNews = (opts?: RequestOptions) =>
  request<NewsProbe>(
    '/api/v0/config/services/news/test',
    { method: 'POST' },
    { timeoutMs: 30000, ...opts },
  )

// ── Web search ──────────────────────────────────────────────────────────

export interface SearchStatus {
  enabled: boolean
  base_url: string
  count: number
  timeout_ms: number
  api_key_set: boolean
  state: string
  detail?: string
  max_results?: number
  revision: string
}

export interface SearchSample {
  title?: string
  url?: string
  date?: string | null
}

export interface SearchProbe {
  ok: boolean
  state: string
  detail?: string
  code?: string
  http_requests: number
  latency_ms: number
  count: number
  samples: SearchSample[]
}

export interface SearchPatch {
  enabled?: boolean
  base_url?: string
  count?: number
  timeout_ms?: number
  api_key?: string
  revision?: string
}

export const getSearch = (opts?: RequestOptions) =>
  request<SearchStatus>('/api/v0/config/services/search', {}, opts)

export const putSearch = (patch: SearchPatch, opts?: RequestOptions) =>
  request<SearchStatus>(
    '/api/v0/config/services/search',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testSearch = (opts?: RequestOptions) =>
  request<SearchProbe>(
    '/api/v0/config/services/search/test',
    { method: 'POST' },
    { timeoutMs: 20000, ...opts },
  )

// ── OpenAPI query adapter ───────────────────────────────────────────────

export interface OpenApiParam {
  /** Read-only spec metadata; omitted by legacy configurations. */
  sensitive?: boolean
  name: string
  location: 'path' | 'query' | 'body'
  required: boolean
  kind: string
  enum_values: string[]
}

export type OpenApiEffectLevel = 'query' | 'mutate' | 'destructive'

export type OpenApiVerificationStrategy =
  | { type: 'readback_op'; op_id: string; json_path: string; expected: unknown }
  | { type: 'response_assert'; json_path: string; expected: unknown }
  | { type: 'transport_only' }

export interface OpenApiOperation {
  id: string
  method: string
  path: string
  summary: string
  params: OpenApiParam[]
  effect_level?: OpenApiEffectLevel
  verification?: OpenApiVerificationStrategy | null
}

export interface OpenApiStatus {
  enabled: boolean
  state: string
  detail: string
  spec_url: string
  base_url: string
  auth_header?: string
  effective_base?: string | null
  token_set: boolean
  operations: OpenApiOperation[]
  timeout_ms: number
  retries: number
  revision: string
}

export interface DiscoveredOperation extends OpenApiOperation {
  callable: boolean
  reason?: string | null
}

export interface OpenApiDiscovery {
  ok: boolean
  state?: string
  code?: string
  detail?: string
  spec?: { title: string; version: string }
  operations?: DiscoveredOperation[]
  http_requests: number
  latency_ms: number
}

export interface OpenApiProbe {
  ok: boolean
  state: string
  code?: string
  detail?: string
  token_set: boolean
  operations: number
  sample?: { operation?: string; status?: number | null }
  http_requests: number
  latency_ms: number
}

export type OpenApiOperationSelection =
  | string
  | {
      id: string
      effect_level?: OpenApiEffectLevel
      verification?: OpenApiVerificationStrategy | null
    }

export interface OpenApiPatch {
  enabled?: boolean
  spec_url?: string
  base_url?: string
  auth_header?: string
  token?: string
  operations?: OpenApiOperationSelection[]
  timeout_ms?: number
  retries?: number
  revision?: string
}

export const getOpenApi = (opts?: RequestOptions) =>
  request<OpenApiStatus>('/api/v0/config/services/openapi', {}, opts)

export const putOpenApi = (patch: OpenApiPatch, opts?: RequestOptions) =>
  request<OpenApiStatus>(
    '/api/v0/config/services/openapi',
    { method: 'PUT', body: JSON.stringify(patch) },
    { timeoutMs: 20000, ...opts },
  )

export const testOpenApi = (opts?: RequestOptions) =>
  request<OpenApiProbe>(
    '/api/v0/config/services/openapi/test',
    { method: 'POST' },
    { timeoutMs: 30000, ...opts },
  )

export const discoverOpenApi = (specUrl: string, token: string | undefined, opts?: RequestOptions) =>
  request<OpenApiDiscovery>(
    '/api/v0/config/services/openapi/discover',
    { method: 'POST', body: JSON.stringify({ spec_url: specUrl, ...(token ? { token } : {}) }) },
    { timeoutMs: 20000, ...opts },
  )

// ── Home automation ─────────────────────────────────────────────────────

export interface HomeEntity {
  name: string
  entity: string
}

export interface HomeStatus {
  backend: 'none' | 'home_assistant'
  entity_source: 'api' | 'exposed'
  availability: 'ready' | 'unconfigured' | 'disabled' | 'hidden'
  state: string
  detail: string
  tools: string[]
  claim: string
  base_url: string
  token_set: boolean
  entities: HomeEntity[]
  timeout_ms: number
  retries: number
}

export interface HomePatch {
  backend?: 'none' | 'home_assistant'
  entity_source?: 'api' | 'exposed'
  base_url?: string
  token?: string
  entities?: HomeEntity[]
  timeout_ms?: number
  retries?: number
}

export interface HomeProbe {
  ok: boolean
  state: 'ready' | 'unconfigured' | 'failed'
  code?: string
  detail?: string
  token_set?: boolean
  entity?: { name?: string; entity?: string; state?: unknown }
  http_requests: number
  latency_ms: number
}

export const getHome = (opts?: RequestOptions) =>
  request<HomeStatus>('/api/v0/config/services/home', {}, opts)

export const putHome = (body: HomePatch, opts?: RequestOptions) =>
  request<HomeStatus>(
    '/api/v0/config/services/home',
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 10000, ...opts },
  )

export const testHome = (opts?: RequestOptions) =>
  request<HomeProbe>(
    '/api/v0/config/services/home/test',
    { method: 'POST' },
    { timeoutMs: 30000, ...opts },
  )

export interface DiscoveredHomeEntity {
  entity: string
  name: string
  state: string
  domain: string
  supported: boolean
}

export interface HomeDiscovery {
  ok: boolean
  state: string
  entity_source?: 'api' | 'exposed'
  ws_commands?: number
  detail?: string
  code?: string
  backend?: string
  token_set?: boolean
  entities: DiscoveredHomeEntity[]
  domains: Record<string, number>
  supported?: number
  http_requests: number
  latency_ms: number
}

export const discoverHome = (opts?: RequestOptions) =>
  request<HomeDiscovery>(
    '/api/v0/config/services/home/discover',
    { method: 'POST' },
    { timeoutMs: 30000, ...opts },
  )

// ── #22 Open Sockets (Declarative Socket Registry) ──────────────────────

export type SocketSideEffect = 'Query' | 'Mutate'
export type OpenSocketMethod = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE'

export interface OpenSocketStatus {
  name: string
  service: string
  description: string
  parameters: Record<string, unknown>
  effect: SocketSideEffect
  endpoint_url: string
  method: OpenSocketMethod
  enabled: boolean
  timeout_ms: number
  auth_header: string
  default_subject?: string
  default_action?: string
  token_set: boolean
  availability: string
}

export interface OpenSocketPatch {
  name?: string
  service?: string
  description?: string
  parameters?: Record<string, unknown>
  effect?: SocketSideEffect
  endpoint_url?: string
  method?: OpenSocketMethod
  enabled?: boolean
  timeout_ms?: number
  auth_header?: string
  default_subject?: string
  default_action?: string
  token?: string
}

export const getOpenSockets = (opts?: RequestOptions) =>
  request<{ sockets: OpenSocketStatus[] }>('/api/v0/config/sockets', {}, opts)

export const createOpenSocket = (body: OpenSocketPatch, opts?: RequestOptions) =>
  request<OpenSocketStatus>(
    '/api/v0/config/sockets',
    { method: 'POST', body: JSON.stringify(body) },
    { timeoutMs: 10000, ...opts },
  )

export const updateOpenSocket = (name: string, body: OpenSocketPatch, opts?: RequestOptions) =>
  request<OpenSocketStatus>(
    `/api/v0/config/sockets/${encodeURIComponent(name)}`,
    { method: 'PUT', body: JSON.stringify(body) },
    { timeoutMs: 10000, ...opts },
  )

export const deleteOpenSocket = (name: string, opts?: RequestOptions) =>
  request<{ removed: boolean; name: string }>(
    '/api/v0/config/sockets',
    { method: 'DELETE', body: JSON.stringify({ name }) },
    { timeoutMs: 10000, ...opts },
  )

// ── Personal Services (story-svc / joke-svc / learning-svc) ─────────────

export interface PersonalEndpointStatus {
  enabled: boolean
  url: string
  timeout_ms: number
  bearer_token_set: boolean
  state: 'ready' | 'unconfigured' | string
}

export interface PersonalEndpointPatch {
  enabled: boolean
  url: string
  timeout_ms?: number
  bearer_token?: string
}

export interface PersonalEndpointProbe {
  ok: boolean
  state: string
  detail?: string
  sample_count?: number
  sample_id?: string
}

export const getStoryConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointStatus>('/api/v0/config/services/story', {}, opts)

export const putStoryConfig = (patch: PersonalEndpointPatch, opts?: RequestOptions) =>
  request<PersonalEndpointStatus>(
    '/api/v0/config/services/story',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testStoryConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointProbe>(
    '/api/v0/config/services/story/test',
    { method: 'POST' },
    { timeoutMs: 15000, ...opts },
  )

export const getJokeConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointStatus>('/api/v0/config/services/joke', {}, opts)

export const putJokeConfig = (patch: PersonalEndpointPatch, opts?: RequestOptions) =>
  request<PersonalEndpointStatus>(
    '/api/v0/config/services/joke',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testJokeConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointProbe>(
    '/api/v0/config/services/joke/test',
    { method: 'POST' },
    { timeoutMs: 15000, ...opts },
  )

export const getRadioConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointStatus>('/api/v0/config/services/radio', {}, opts)

export const putRadioConfig = (patch: PersonalEndpointPatch, opts?: RequestOptions) =>
  request<PersonalEndpointStatus>(
    '/api/v0/config/services/radio',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testRadioConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointProbe>(
    '/api/v0/config/services/radio/test',
    { method: 'POST' },
    { timeoutMs: 15000, ...opts },
  )

export const getLearningConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointStatus>('/api/v0/config/services/learning', {}, opts)

export const putLearningConfig = (patch: PersonalEndpointPatch, opts?: RequestOptions) =>
  request<PersonalEndpointStatus>(
    '/api/v0/config/services/learning',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testLearningConfig = (opts?: RequestOptions) =>
  request<PersonalEndpointProbe>(
    '/api/v0/config/services/learning/test',
    { method: 'POST' },
    { timeoutMs: 15000, ...opts },
  )

// ── Master Brain (#267) ─────────────────────────────────────────────────

export type MasterBrainProtocol = 'hermes' | 'generic'

export interface MasterBrainTaskRecord {
  task_id: string
  summary: string
  device?: string
  room?: string
  status: string
  last_tool?: string
  pending_approval?: string
  created_ms: number
  updated_ms: number
}

export interface MasterBrainStatus {
  enabled: boolean
  protocol: MasterBrainProtocol
  url: string
  callback_url: string
  timeout_ms: number
  bearer_token_set: boolean
  state: 'ready' | 'disabled' | 'unconfigured' | string
  recent_tasks: MasterBrainTaskRecord[]
}

export interface MasterBrainPatch {
  enabled: boolean
  protocol?: MasterBrainProtocol
  url: string
  callback_url?: string
  timeout_ms?: number
  bearer_token?: string
}

export interface MasterBrainProbe {
  ok: boolean
  protocol?: MasterBrainProtocol
  state: string
  latency_ms?: number
  code?: string
  detail?: string
  health?: Record<string, unknown>
}

export const getMasterBrainConfig = (opts?: RequestOptions) =>
  request<MasterBrainStatus>('/api/v0/config/services/master_brain', {}, opts)

export const putMasterBrainConfig = (patch: MasterBrainPatch, opts?: RequestOptions) =>
  request<MasterBrainStatus>(
    '/api/v0/config/services/master_brain',
    { method: 'PUT', body: JSON.stringify(patch) },
    opts,
  )

export const testMasterBrainConfig = (opts?: RequestOptions) =>
  request<MasterBrainProbe>(
    '/api/v0/config/services/master_brain/test',
    { method: 'POST' },
    { timeoutMs: 15000, ...opts },
  )
