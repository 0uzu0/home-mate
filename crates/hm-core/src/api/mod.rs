//! Management API (v0) — the console's real backend, per docs/contracts-v0.md.

use crate::agent::AgentEnv;
use crate::config::{ConfigService, ServiceConfig};
use crate::llm::LlmClient;
use crate::session::SessionStore;
use crate::task::TaskRegistry;
use crate::tool::mocks::MockInject;
use crate::tool::Registry;
use crate::trace::TraceRecorder;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

mod devices;
pub(crate) mod services;
mod voice;

pub use devices::DeviceMuteBody;
use devices::*;
use services::*;
pub use services::{
    apply_config_import, build_config_document, ConfigImportBody, ImportOutcome,
    FLEET_SOURCE_HEADER,
};
use voice::*;

pub struct AppState {
    pub store: Arc<crate::store::Store>,
    /// M9: household-plane bearer tokens. One middleware enforces them on
    /// every authenticated `/api/v0/*` path (see `crate::auth`).
    pub auth: Arc<crate::auth::AuthStore>,
    pub execution: crate::execution::ScopeBindings,
    pub confirmations: Arc<crate::confirmation::ConfirmationService>,
    /// M12a-1: the configured peers and the config push (see `crate::fleet`).
    pub fleet: Arc<crate::fleet::FleetService>,
    pub config: ConfigService,
    pub registry: Arc<Registry>,
    pub sessions: Arc<SessionStore>,
    pub tasks: Arc<TaskRegistry>,
    pub traces: Arc<TraceRecorder>,
    pub hub: Arc<crate::gateway::DeviceHub>,
    pub voice: Arc<crate::voice::VoicePipeline>,
    /// The user-configured music service (R1e).
    pub music: Arc<crate::tool::music::MusicRuntime>,
    /// The user-configured weather service (R1h): the first service that is
    /// registered like any other and obeys the same interaction rules.
    pub weather: Arc<crate::tool::weather::WeatherRuntime>,
    /// Home automation (R1j/P0-b): registered, and honest while no backend
    /// exists. The console card renders its state word; the tool refuses
    /// with HOME_UNCONFIGURED instead of pretending a light changed.
    pub home: Arc<crate::tool::home::HomeRuntime>,
    /// News feeds (R2-BM6): registered, unconfigured until a household adds
    /// a source. No sample feed exists.
    pub news: Arc<crate::tool::news::NewsRuntime>,
    /// Generic OpenAPI query adapter (R2-BM6): one tool, a stored whitelist
    /// and a write-only bearer token.
    pub openapi: Arc<crate::tool::openapi::OpenApiRuntime>,
    /// #22 Declarative Open Socket Registry: dynamic HTTP tool contracts.
    pub open_sockets: Arc<crate::tool::open_socket::OpenSocketRuntime>,
    /// General web information search (M2.5): registered, unconfigured until
    /// a household stores an endpoint and a key — zero HTTP before that.
    pub search: Arc<crate::tool::search::SearchRuntime>,
    /// Phase C: NAS `story-svc` (:18083) and `joke-svc` (:18081) runtime.
    pub personal_media: crate::tool::personal_media::PersonalMediaRuntime,
    /// Phase C: NAS `learning-svc` (#119-#123) runtime.
    pub learning: crate::tool::learning::LearningRuntime,
    /// #267: provider-agnostic Master Brain async delegation runtime.
    pub master_brain: Arc<crate::tool::master_brain::MasterBrainRuntime>,
    /// R2-BM6: `system.volume` / `system.duplex` reach the same pipeline the
    /// console's volume/mode endpoints use.
    pub management: Arc<crate::tool::management::ManagementRuntime>,
    /// 2026-11-13: short, user-confirmed household notes (xiaozhi-style). The
    /// console owns CRUD; the prompt gets a bounded block; operational state
    /// is explicitly NOT memory (it belongs to config/state).
    pub memory: Arc<crate::memory::MemoryService>,
    /// M6 (#23): long-term working memory (project paths, progress, context).
    pub working_memory: Arc<crate::working_memory::WorkingMemoryStore>,
    /// W3 (#80): offline-resilient shared memory outbox & summary cache.
    pub shared_memory: Arc<crate::shared_memory::SharedMemoryService>,
    /// W4b (#83): versioned Agent.md & auditable USER.md stores.
    pub agent_md: Arc<crate::agent_md::AgentMdService>,
    pub user_md: Arc<crate::user_md::UserMdService>,
    pub assistant_name: String,
    pub data_dir: PathBuf,
    /// Tests inject a scripted client here; production builds the LLM per
    /// request from the current config (so config changes apply live).
    pub llm_override: Option<Arc<dyn LlmClient>>,
    /// Same for TTS in voice-pipeline tests.
    pub tts_override: Option<Arc<dyn crate::speech::TtsClient>>,
    /// Built console directory (web/dist). When present the core serves the
    /// console itself, so a deployment needs a single port (NAS M4 shape);
    /// `None` = API only (dev uses `npm run dev` on :8931).
    pub web_dir: Option<PathBuf>,
    /// On-speaker A/B slot layouts for the §12 update API (audio firmware +
    /// core itself). Unavailable (dir missing) on dev boxes — the API then
    /// reports availability=false instead of erroring.
    pub fw_audio: crate::fwupdate::SlotLayout,
    pub fw_core: crate::fwupdate::SlotLayout,
}

/// Build an AgentEnv from live components (shared by the HTTP chat path and
/// the voice pipeline's device turns — one brain, both entry points).
pub fn build_agent_env(
    config: &ConfigService,
    registry: Arc<Registry>,
    sessions: Arc<SessionStore>,
    tasks: Arc<TaskRegistry>,
    traces: Arc<TraceRecorder>,
    memory: Arc<crate::memory::MemoryService>,
    working_memory: Arc<crate::working_memory::WorkingMemoryStore>,
    shared_memory: Option<Arc<crate::shared_memory::SharedMemoryService>>,
    user_md: Option<Arc<crate::user_md::UserMdService>>,
    assistant_name: &str,
    llm_override: Option<Arc<dyn LlmClient>>,
) -> Result<Arc<AgentEnv>, String> {
    let runtime = config.runtime_snapshot()?;
    let cfg = &runtime.config;
    let llm: Arc<dyn LlmClient> = match llm_override {
        Some(c) => c,
        None => runtime.llm_client("llm")?,
    };
    // Secondary LLM (user request 2026-11-10): constructed only when fully
    // configured — one attempt after the primary's retries give up.
    let llm_fallback = if cfg.llm_fallback.is_configured() && !cfg.llm_fallback.base_url.is_empty()
    {
        match runtime.llm_client("llm_fallback") {
            Ok(c) => Some(crate::agent::FallbackLlm {
                client: c,
                model: cfg.llm_fallback.model.clone(),
                label: format!("{}/{}", cfg.llm_fallback.provider, cfg.llm_fallback.model),
            }),
            Err(e) => {
                tracing::warn!("llm_fallback client construction failed: {e}");
                None
            }
        }
    } else {
        None
    };
    let prompt_preferences = config.prompt_preferences()?;
    let effective_assistant_name = if prompt_preferences.assistant_name.trim().is_empty() {
        assistant_name.to_string()
    } else {
        prompt_preferences.assistant_name.trim().to_string()
    };
    Ok(Arc::new(AgentEnv {
        prompt_preferences,
        llm,
        model: cfg.llm.model.clone(),
        llm_fallback,
        registry,
        sessions,
        tasks,
        traces,
        memory,
        working_memory,
        shared_memory,
        user_md,
        scope: crate::working_memory::ScopeContext::default(),
        assistant_name: effective_assistant_name,
        llm_first_event_timeout: std::time::Duration::from_secs(35),
    }))
}

impl AppState {
    pub fn new(
        store: Arc<crate::store::Store>,
        data_dir: PathBuf,
        inject: MockInject,
        llm_override: Option<Arc<dyn LlmClient>>,
    ) -> Arc<Self> {
        Self::with_overrides(store, data_dir, inject, llm_override, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_overrides(
        store: Arc<crate::store::Store>,
        data_dir: PathBuf,
        inject: MockInject,
        llm_override: Option<Arc<dyn LlmClient>>,
        tts_override: Option<Arc<dyn crate::speech::TtsClient>>,
        asr_override: Option<Arc<dyn crate::speech::AsrClient>>,
        web_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let config = ConfigService::new(store.clone());
        let auth = crate::auth::AuthStore::new(store.clone());
        let fleet = crate::fleet::FleetService::new(store.clone());
        // Serial/root recovery: a leftover `<data_dir>/auth-reset` returns the
        // API to the unpaired state. Checked before anything can serve.
        auth.apply_reset_sentinel(&data_dir);
        let _ = crate::release::ensure_local_token(&data_dir);
        let sessions = Arc::new(SessionStore::new(store.clone()));
        let tasks = Arc::new(TaskRegistry::new(store.clone()));
        let traces = Arc::new(TraceRecorder::new(store.clone()));
        let hub = crate::gateway::DeviceHub::new();
        hub.attach_auth(auth.clone());

        // Music (R1e): a real, user-configured service. Its events join the
        // voice event ring below, so the console and the acceptance tools see
        // "which song, which source, confirmed?" in one place.
        let music = crate::tool::music::MusicRuntime::new(store.clone(), hub.clone());
        let weather = crate::tool::weather::WeatherRuntime::new(store.clone());
        let home = crate::tool::home::HomeRuntime::new(store.clone());
        let news = crate::tool::news::NewsRuntime::new(store.clone());
        let personal_media =
            crate::tool::personal_media::PersonalMediaRuntime::new(store.clone(), hub.clone())
                .with_music(music.clone());
        let learning =
            crate::tool::learning::LearningRuntime::new(store.clone()).with_hub(hub.clone());
        let master_brain = crate::tool::master_brain::MasterBrainRuntime::new(store.clone());
        let openapi = crate::tool::openapi::OpenApiRuntime::new(store.clone());
        let open_sockets = crate::tool::open_socket::OpenSocketRuntime::new(store.clone());
        let search = crate::tool::search::SearchRuntime::new(store.clone());
        // R2-BM6: management tools share the live VoicePipeline; the Weak
        // handle is attached right after `voice.start()`.
        let management =
            crate::tool::management::ManagementRuntime::new(store.clone(), hub.clone());
        let memory = crate::memory::MemoryService::new(store.clone());
        let working_memory = crate::working_memory::WorkingMemoryStore::new(store.clone());
        let shared_memory = crate::shared_memory::SharedMemoryService::new(store.clone());
        let agent_md = Arc::new(crate::agent_md::AgentMdService::new(store.clone()));
        let user_md = Arc::new(crate::user_md::UserMdService::new(store.clone()));
        if tokio::runtime::Handle::try_current().is_ok() {
            shared_memory.spawn_background_sync();
        }
        let mut reg = crate::tool::mocks::build_registry(
            inject,
            music.clone(),
            weather.clone(),
            home.clone(),
            news.clone(),
            openapi.clone(),
            management.clone(),
        );
        reg.register(Arc::new(
            crate::tool::memory::MemoryTool::new(memory.clone())
                .with_shared_memory(shared_memory.clone()),
        ))
        .expect("valid unique tool registration");
        reg.register(Arc::new(
            crate::tool::memory::WorkingMemorySetTool::new(working_memory.clone())
                .with_shared_memory(shared_memory.clone()),
        ))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::memory::WorkingMemoryGetTool::new(
            working_memory.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::memory::WorkingMemoryDeleteTool::new(
            working_memory.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(
            crate::tool::memory::UserProfileProposeTool::new(user_md.clone())
                .with_shared_memory(shared_memory.clone()),
        ))
        .expect("valid unique tool registration");
        // Reminders (2026-11-14): voice-set timed reminders. The tool, the
        // console and the firing sweep all read the same store rows.
        let reminders = Arc::new(crate::reminder::ReminderService::new(store.clone()));
        let reminder_rt = crate::tool::reminder::ReminderRuntime::new(reminders, hub.clone());
        reg.register(Arc::new(crate::tool::reminder::ReminderSetTool::new(
            reminder_rt.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::reminder::ReminderListTool::new(
            reminder_rt.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::reminder::ReminderCancelTool::new(
            reminder_rt,
        )))
        .expect("valid unique tool registration");
        // M2.5: web search is registered like any other service. Dispatch
        // works while unconfigured; the tool itself refuses with
        // SEARCH_UNCONFIGURED and the prompt names it as not ready.
        reg.register(Arc::new(crate::tool::search::WebSearchTool::new(
            search.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::personal_media::JokeGetTool::new(
            personal_media.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::personal_media::StorySearchTool::new(
            personal_media.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::personal_media::StoryAlbumTool::new(
            personal_media.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::personal_media::StoryPlayTool::new(
            personal_media.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::personal_media::RadioSearchTool::new(
            personal_media.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::personal_media::RadioPlayTool::new(
            personal_media.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::learning::LearningDecksTool::new(
            learning.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::learning::LearningStartTool::new(
            learning.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::learning::LearningCurrentTool::new(
            learning.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::learning::LearningAnswerTool::new(
            learning.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::learning::LearningRevealTool::new(
            learning.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::learning::LearningControlTool::new(
            learning.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::master_brain::BrainDelegateTool::new(
            master_brain.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::master_brain::BrainStatusTool::new(
            master_brain.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::master_brain::BrainSteerTool::new(
            master_brain.clone(),
        )))
        .expect("valid unique tool registration");
        reg.register(Arc::new(crate::tool::master_brain::BrainCancelTool::new(
            master_brain.clone(),
        )))
        .expect("valid unique tool registration");
        if let Err(error) = reg.attach_provider_for_repair(open_sockets.clone()) {
            tracing::error!(%error, "persisted tool catalog invalid; management repair remains available");
        }
        let registry = Arc::new(reg);

        let env_factory: crate::voice::EnvFactory = {
            let store = store.clone();
            let registry = registry.clone();
            let sessions = sessions.clone();
            let tasks = tasks.clone();
            let traces = traces.clone();
            let llm_override = llm_override.clone();
            let memory = memory.clone();
            let working_memory = working_memory.clone();
            let shared_memory = shared_memory.clone();
            let user_md = user_md.clone();
            Arc::new(move || {
                let config = ConfigService::new(store.clone());
                build_agent_env(
                    &config,
                    registry.clone(),
                    sessions.clone(),
                    tasks.clone(),
                    traces.clone(),
                    memory.clone(),
                    working_memory.clone(),
                    Some(shared_memory.clone()),
                    Some(user_md.clone()),
                    crate::ASSISTANT_NAME,
                    llm_override.clone(),
                )
            })
        };
        let tts_factory: crate::voice::TtsFactory = {
            let store = store.clone();
            let tts_override = tts_override.clone();
            Arc::new(move || {
                if let Some(t) = &tts_override {
                    return Ok(t.clone() as Arc<dyn crate::speech::TtsClient>);
                }
                let config = ConfigService::new(store.clone());
                config.runtime_tts()
            })
        };
        let asr_factory: crate::voice::AsrFactory = {
            let store = store.clone();
            let asr_override = asr_override.clone();
            Arc::new(move || {
                if let Some(a) = &asr_override {
                    return Ok(a.clone() as Arc<dyn crate::speech::AsrClient>);
                }
                let config = ConfigService::new(store.clone());
                config.runtime_asr()
            })
        };
        let voice = crate::voice::VoicePipeline::new(
            hub.clone(),
            store.clone(),
            tasks.clone(),
            env_factory,
            tts_factory,
            asr_factory,
            data_dir.clone(),
        );
        // M7-1: an INJECTED brain/voice (the test seam) also disables the
        // session-end memory extractor. It spends two real model calls, and a
        // test whose scripted responses are about turns must not have them
        // consumed by background work; the memory suite enables it explicitly.
        if llm_override.is_some() || tts_override.is_some() || asr_override.is_some() {
            voice.set_memory_extraction(false);
        }
        voice.start();
        // R2-BM6: now that the pipeline exists, let the management tools reach
        // it. Same methods as the console: no second implementation.
        management.attach_voice(&voice);
        master_brain.attach_voice(&voice);
        // M3: the two runtimes point at each other (weak). The resolver needs
        // the program bookmarks; the wake path needs to PAUSE media instead of
        // discarding it.
        music.attach_voice(&voice);
        voice.attach_music(&music);
        // Contract v2 #2: cues travel between the house's cores through the
        // fleet plane (each device runs its own core, so a neighbour's cue is
        // not a local fact until it is told).
        voice.attach_fleet(&fleet);
        voice.attach_learning(&learning);
        // Music facts join the voice event ring (one timeline for the console,
        // the diagnostics page and the acceptance tools).
        {
            let voice = voice.clone();
            music.set_event_sink(Arc::new(move |kind: &str, payload: serde_json::Value| {
                voice.log_event(kind, payload);
            }));
        }
        {
            let voice = voice.clone();
            weather.set_event_sink(Arc::new(move |kind: &str, payload: serde_json::Value| {
                voice.log_event(kind, payload);
            }));
        }
        {
            let voice = voice.clone();
            news.set_event_sink(Arc::new(move |kind: &str, payload: serde_json::Value| {
                voice.log_event(kind, payload);
            }));
        }
        {
            let voice = voice.clone();
            search.set_event_sink(Arc::new(move |kind: &str, payload: serde_json::Value| {
                voice.log_event(kind, payload);
            }));
        }

        let confirmations = crate::confirmation::ConfirmationService::new(
            store.clone(),
            tasks.clone(),
            registry.clone(),
            openapi.clone(),
        );
        openapi.attach_confirmations(&confirmations);
        Arc::new(Self {
            confirmations,
            execution: crate::execution::ScopeBindings::new(
                store.clone(),
                auth.clone(),
                sessions.clone(),
            ),
            sessions,
            tasks,
            traces,
            auth,
            fleet,
            config,
            personal_media,
            learning,
            master_brain,
            registry,
            hub,
            voice,
            music,
            weather,
            home,
            news,
            openapi,
            open_sockets,
            search,
            management,
            memory: memory.clone(),
            working_memory: working_memory.clone(),
            shared_memory: shared_memory.clone(),
            agent_md: agent_md.clone(),
            user_md: user_md.clone(),
            assistant_name: crate::ASSISTANT_NAME.into(),
            data_dir,
            store,
            llm_override,
            tts_override,
            web_dir,
            fw_audio: crate::fwupdate::SlotLayout::audio(),
            fw_core: crate::fwupdate::SlotLayout::core(),
        })
    }

    pub fn effective_assistant_name(&self) -> String {
        self.config
            .prompt_preferences()
            .ok()
            .map(|p| p.assistant_name.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.assistant_name.clone())
    }

    pub fn agent_env(&self) -> Result<Arc<AgentEnv>, String> {
        build_agent_env(
            &self.config,
            self.registry.clone(),
            self.sessions.clone(),
            self.tasks.clone(),
            self.traces.clone(),
            self.memory.clone(),
            self.working_memory.clone(),
            Some(self.shared_memory.clone()),
            Some(self.user_md.clone()),
            &self.assistant_name,
            self.llm_override.clone(),
        )
    }

    /// Start the device-gateway listener (production entry; tests bind
    /// their own ephemeral hubs).
    pub fn start_gateway_listener(
        &self,
        addr: std::net::SocketAddr,
    ) -> tokio::task::JoinHandle<()> {
        let hub = self.hub.clone();
        tokio::spawn(async move {
            if let Err(e) = hub.listen(addr).await {
                tracing::error!(error = %e, "gateway listener failed");
                panic!("gateway listener failed: {e}");
            }
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let auth_state = state.clone();
    Router::new()
        // M9: household-plane authentication. The two bootstrap paths are
        // exempt inside the middleware (a device with no token must be
        // pair-able, and its console bundle must load before pairing).
        .route("/api/v0/auth", get(auth_status))
        .route("/api/v0/auth/provision", post(auth_provision))
        .route("/api/v0/auth/rotate", post(auth_rotate))
        .route("/api/v0/auth/lan-exempt", put(auth_lan_exempt_put))
        .route("/api/v0/chat", post(chat))
        .route("/api/v0/session/cancel", post(cancel))
        .route("/api/v0/session/{id}/messages", get(session_messages))
        .route("/api/v0/session/{id}/clear", post(session_clear))
        .route(
            "/api/v0/devices/{id}/conversation",
            get(device_conversation).delete(device_conversation_clear),
        )
        .route("/api/v0/trace", get(trace_list))
        .route("/api/v0/trace/{id}", get(trace_detail))
        .route("/api/v0/tasks", get(tasks_list))
        .route("/api/v0/status", get(status))
        .route("/api/v0/config/services", get(config_services))
        .route(
            "/api/v0/config/services/music",
            get(config_music_get).put(config_music_put),
        )
        .route(
            "/api/v0/config/services/music/test",
            post(config_music_test),
        )
        .route(
            "/api/v0/config/services/weather",
            get(config_weather_get).put(config_weather_put),
        )
        .route(
            "/api/v0/config/services/weather/test",
            post(config_weather_test),
        )
        .route(
            "/api/v0/config/services/weather/geocode",
            get(config_weather_geocode),
        )
        .route(
            "/api/v0/config/services/home",
            get(config_home_get).put(config_home_put),
        )
        .route("/api/v0/config/services/home/test", post(config_home_test))
        .route(
            "/api/v0/config/services/home/discover",
            post(config_home_discover),
        )
        .route(
            "/api/v0/config/services/news",
            get(config_news_get).put(config_news_put),
        )
        .route("/api/v0/config/services/news/test", post(config_news_test))
        .route(
            "/api/v0/config/services/story",
            get(config_story_get).put(config_story_put),
        )
        .route(
            "/api/v0/config/services/story/test",
            post(config_story_test),
        )
        .route(
            "/api/v0/config/services/joke",
            get(config_joke_get).put(config_joke_put),
        )
        .route("/api/v0/config/services/joke/test", post(config_joke_test))
        .route(
            "/api/v0/config/services/radio",
            get(config_radio_get).put(config_radio_put),
        )
        .route(
            "/api/v0/config/services/radio/test",
            post(config_radio_test),
        )
        .route(
            "/api/v0/config/services/learning",
            get(config_learning_get).put(config_learning_put),
        )
        .route(
            "/api/v0/config/services/learning/test",
            post(config_learning_test),
        )
        .route(
            "/api/v0/config/services/master_brain",
            get(config_master_brain_get).put(config_master_brain_put),
        )
        .route(
            "/api/v0/config/services/master_brain/test",
            post(config_master_brain_test),
        )
        .route(
            "/api/v0/config/master-brain",
            get(config_master_brain_get).put(config_master_brain_put),
        )
        .route(
            "/api/v0/config/master-brain/test",
            post(config_master_brain_test),
        )
        .route(
            "/api/v0/config/services/search",
            get(config_search_get).put(config_search_put),
        )
        .route(
            "/api/v0/config/services/search/test",
            post(config_search_test),
        )
        .route(
            "/api/v0/config/services/openapi",
            get(config_openapi_get).put(config_openapi_put),
        )
        .route(
            "/api/v0/config/services/openapi/test",
            post(config_openapi_test),
        )
        .route(
            "/api/v0/config/services/openapi/discover",
            post(config_openapi_discover),
        )
        .route(
            "/api/v0/config/sockets",
            get(config_sockets_get)
                .post(config_sockets_post)
                .delete(config_sockets_remove),
        )
        .route(
            "/api/v0/config/sockets/{name}",
            put(config_sockets_put).delete(config_sockets_delete),
        )
        .route("/api/v0/reminders", get(reminders_get).post(reminders_add))
        .route("/api/v0/reminders/{id}", delete(reminder_delete))
        .route("/api/v0/memories", get(memories_get).post(memories_add))
        .route("/api/v0/memories/clear", post(memories_clear))
        .route("/api/v0/memories/candidates", get(memory_candidates_get))
        .route(
            "/api/v0/memories/candidates/{id}/confirm",
            post(memory_candidate_confirm),
        )
        .route(
            "/api/v0/memories/candidates/{id}/reject",
            post(memory_candidate_reject),
        )
        .route(
            "/api/v0/memories/candidates/{id}",
            delete(memory_candidate_delete),
        )
        .route(
            "/api/v0/memories/{id}",
            put(memories_update).delete(memories_delete),
        )
        .route(
            "/api/v0/working-memory",
            get(working_memory_get)
                .put(working_memory_put)
                .delete(working_memory_delete_by_key),
        )
        .route("/api/v0/working-memory/clear", post(working_memory_clear))
        .route("/api/v0/working-memory/{id}/pin", post(working_memory_pin))
        .route("/api/v0/working-memory/{id}", delete(working_memory_delete))
        .route("/api/v0/shared-memory/status", get(shared_memory_status))
        .route(
            "/api/v0/shared-memory/config",
            put(shared_memory_config_put),
        )
        .route(
            "/api/v0/shared-memory/clear-outbox",
            post(shared_memory_clear_outbox),
        )
        .route("/api/v0/agent-md", get(agent_md_get).put(agent_md_put))
        .route("/api/v0/agent-md/rollback", post(agent_md_rollback))
        .route("/api/v0/user-md", get(user_md_get).put(user_md_put))
        .route("/api/v0/user-md/{id}/revert", post(user_md_revert))
        .route(
            "/api/v0/config/audio",
            get(config_audio_get).put(config_audio_put),
        )
        .route("/api/v0/reflex", get(reflex_get).put(reflex_put))
        .route("/api/v0/reflex/tokens", get(reflex_tokens_get))
        .route("/api/v0/reflex/deploy", post(reflex_deploy))
        .route("/api/v0/device-meta", get(device_meta_get))
        .route("/api/v0/device-meta/{id}", put(device_meta_put))
        .route("/api/v0/system/restart", post(system_restart))
        .route("/api/v0/system/factory-reset", post(system_factory_reset))
        .route("/api/v0/config/services/{svc}", put(config_service_put))
        .route(
            "/api/v0/config/services/{svc}/test",
            post(config_service_test),
        )
        .route("/api/v0/config/history", get(config_history))
        .route(
            "/api/v0/config/history/{id}/restore",
            post(config_history_restore),
        )
        .route(
            "/api/v0/config/export",
            get(config_export).post(config_export),
        )
        .route("/api/v0/config/import", post(config_import))
        // M12a-1: peers and configuration push (docs/contracts-v0.md v0.26).
        .route("/api/v0/fleet", get(fleet_get).put(fleet_self_put))
        .route("/api/v0/fleet/ping", post(fleet_ping))
        .route("/api/v0/fleet/push", post(fleet_push))
        // v0.30 (wake arbitration): one wake claim, exchanged between nodes.
        // Behind the same bearer as every other /api/v0 path, so a peer
        // without a token simply cannot answer (which is the documented
        // degradation, not a silent one).
        .route("/api/v0/fleet/wake", post(fleet_wake))
        .route("/api/v0/fleet/cue", post(fleet_cue))
        .route("/api/v0/fleet/playback", post(fleet_playback))
        .route("/api/v0/fleet/owner", post(fleet_owner))
        .route(
            "/api/v0/fleet/{id}",
            put(fleet_node_put).delete(fleet_node_delete),
        )
        .route("/api/v0/devices", get(devices))
        .route("/api/v0/devices/{id}/stop", post(device_stop))
        .route(
            "/api/v0/devices/{id}/program/pause",
            post(device_program_pause),
        )
        .route(
            "/api/v0/devices/{id}/program/resume",
            post(device_program_resume),
        )
        .route(
            "/api/v0/devices/{id}/media/control",
            post(device_media_control),
        )
        .route("/api/v0/devices/{id}/volume", post(device_volume))
        // Diagnostic mic capture: the post-AEC mono the SPOTTER consumes, so
        // a KWS hit-rate measurement needs no firmware flag and no extra mic.
        .route(
            "/api/v0/devices/{id}/mic/capture",
            get(device_mic_capture_status).post(device_mic_capture_start),
        )
        .route(
            "/api/v0/devices/{id}/mic/capture.wav",
            get(device_mic_capture_wav),
        )
        .route("/api/v0/devices/{id}/wifi", post(device_wifi))
        .route("/api/v0/devices/{id}/mute", put(device_mute))
        .route(
            "/api/v0/devices/{id}/profile",
            get(device_profile_get).put(device_profile_put),
        )
        .route("/api/v0/mode", get(mode_get).put(mode_put_global))
        .route("/api/v0/session/close", post(session_close))
        .route("/api/v0/prompts", put(prompts_put))
        .route("/api/v0/mode/{device}", put(mode_put))
        .route("/api/v0/voice/log", get(voice_log))
        .route("/api/v0/voice/events", get(voice_events))
        .route(
            "/api/v0/voice/announce",
            get(voice_announce_get).post(voice_announce_post),
        )
        .route("/api/v0/diagnostics", get(diagnostics_get))
        .route(
            "/api/v0/diagnostics/self_check",
            post(diagnostics_self_check),
        )
        .route(
            "/api/v0/diagnostics/tests/voice_play",
            post(diagnostics_voice_play),
        )
        .route(
            "/api/v0/diagnostics/tests/play_asset",
            post(diagnostics_play_asset),
        )
        .route(
            "/api/v0/diagnostics/tests/playback_loopback",
            post(diagnostics_playback_loopback),
        )
        .route(
            "/api/v0/diagnostics/tests/mics",
            post(diagnostics_mic_array),
        )
        .route(
            "/api/v0/diagnostics/repair/reapply_mode",
            post(diagnostics_repair_mode),
        )
        .route("/api/v0/firmware", get(firmware_get))
        .route(
            "/api/v0/firmware/audio",
            post(firmware_audio_update)
                .layer(axum::extract::DefaultBodyLimit::max(FIRMWARE_MAX_BYTES)),
        )
        .route(
            "/api/v0/firmware/core",
            post(firmware_core_update)
                .layer(axum::extract::DefaultBodyLimit::max(FIRMWARE_MAX_BYTES)),
        )
        // Release management (M8a): the core only orchestrates; verification
        // and pointer writes belong to the separate hm-release binary.
        .route("/api/v0/release", get(release_get))
        .route("/api/v0/release/install", post(release_install))
        .route("/api/v0/release/activate", post(release_activate))
        .route("/api/v0/release/rollback", post(release_rollback))
        .route("/api/v0/release/log", get(release_log))
        .route("/api/v0/release/snapshot", post(release_snapshot))
        .merge(crate::config_delivery::api::routes())
        .merge(crate::execution::api::routes())
        .merge(crate::confirmation::api::routes())
        .fallback(console_fallback)
        // M9: one gate for every /api path (static console files stay open so
        // the SPA can load before the browser has a token).
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_bearer,
        ))
        .layer(axum::middleware::from_fn(api_security_headers))
        .with_state(state)
}

async fn api_security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    if is_api {
        headers.insert(
            axum::http::header::X_FRAME_OPTIONS,
            axum::http::HeaderValue::from_static("DENY"),
        );
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    } else {
        headers.insert(
            axum::http::header::X_FRAME_OPTIONS,
            axum::http::HeaderValue::from_static("SAMEORIGIN"),
        );
    }
    resp
}

// --- M9: household-plane bearer authentication --------------------------------

/// `WWW-Authenticate` value (RFC 6750).
const BEARER_REALM: &str = "Bearer realm=\"home-mate\"";

fn unauthorized(detail: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized", "detail": detail })),
    )
        .into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(BEARER_REALM) {
        resp.headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, v);
    }
    resp
}

fn forbidden(detail: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "forbidden", "detail": detail })),
    )
        .into_response()
}

fn too_many_requests(retry_after_secs: u64) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "error": "too_many_requests",
            "detail": "too many failed authentication attempts",
            "retry_after_secs": retry_after_secs,
        })),
    )
        .into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after_secs.to_string()) {
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, v);
    }
    resp
}

/// Throttled denial log: a LAN scan must not fill /run, but the first denied
/// request of each window has to be attributable.
fn log_denied(peer: Option<std::net::SocketAddr>, path: &str, reason: &str, failures: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    let now = crate::id::now_ms();
    let last = LAST_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 5_000 {
        return;
    }
    LAST_MS.store(now, Ordering::Relaxed);
    tracing::warn!(
        peer = %peer.map(|p| p.to_string()).unwrap_or_else(|| "unknown".into()),
        path,
        reason,
        failures,
        "api_auth_denied"
    );
}

/// The single auth gate. Order matters:
/// 1. non-API paths (console bundle) pass — a navigation cannot carry a header;
/// 2. never paired (`Ok(None)`):
///    - loopback (and in-process router tests without ConnectInfo) stays open;
///    - non-private / public IPs are rejected outright with `403 Forbidden`
///      (including `/api/v0/auth/provision`);
///    - private LAN / Tailscale / IPv6 ULA peers are blocked (`401`) on
///      high-risk irreversible/secret-exfiltration routes while keeping
///      read-only status/auth and one-shot `/api/v0/auth/provision` usable;
/// 3. once paired (`Ok(Some(list))`), the two bootstrap endpoints pass
///    (`/api/v0/auth` hides token metadata; `/api/v0/auth/provision` returns 409);
/// 4. loopback → trusted for legacy `hm-release` routes;
/// 5. otherwise require `Authorization: Bearer hm1_…` (or private-LAN everyday
///    allowlist when `lan_gateway_exempt = true`), with per-IP failure rate limit.
async fn require_bearer(
    State(state): State<Arc<AppState>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0);
    let is_loopback = peer.is_some_and(|p| crate::net::is_loopback_ip(p.ip()));
    let entries = match state.auth.entries() {
        Ok(None) => {
            if peer.is_none() || is_loopback {
                return next.run(req).await;
            }
            let ip = peer.unwrap().ip();
            let allowed_lan = crate::net::is_private_lan_ip(ip) || crate::net::is_ipv6_ula(ip);
            if !allowed_lan {
                log_denied(peer, &path, "unpaired core rejected non-private peer", 0);
                return forbidden(
                    "unpaired core only accepts loopback or private LAN / Tailscale peers",
                );
            }
            if is_unpaired_lan_blocked_route(req.method(), &path) {
                log_denied(peer, &path, "unpaired LAN blocked high-risk route", 0);
                return unauthorized(
                    "pair a household token before calling this endpoint from LAN",
                );
            }
            return next.run(req).await;
        }
        Ok(Some(list)) => list,
        Err(e) => {
            if path == "/api/v0/auth" {
                return next.run(req).await;
            }
            // Fail closed: a corrupt store must not silently reopen the API.
            log_denied(peer, &path, &format!("token store unreadable: {e}"), 0);
            return unauthorized("token store unreadable; recovery requires root");
        }
    };
    if path == "/api/v0/auth" || path == "/api/v0/auth/provision" {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if is_loopback {
        if !presented.is_empty() {
            if crate::release::verify_local_token(&state.data_dir, presented) {
                return next.run(req).await;
            }
            if let Some(id) = crate::auth::verify_in(&entries, presented) {
                state.auth.touch(&id);
                return next.run(req).await;
            }
            log_denied(peer, &path, "loopback bearer token did not verify", 0);
            return unauthorized("pair this client again (invalid or rotated token)");
        }
        if is_legacy_loopback_release_route(req.method(), &path) {
            return next.run(req).await;
        }
        log_denied(
            peer,
            &path,
            "no bearer token on non-release loopback route",
            0,
        );
        return unauthorized("pair this client again (invalid or rotated token)");
    }
    if let Some(ip) = peer.map(|p| p.ip()) {
        if let Some(retry_after) = state.auth.is_rate_limited(ip) {
            return too_many_requests(retry_after);
        }
    }
    if !req
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION)
        && state.auth.lan_gateway_exempt()
        && peer
            .map(|p| crate::net::is_private_lan_ip(p.ip()))
            .unwrap_or(false)
        && is_lan_exempt_everyday_route(req.method(), &path)
    {
        req.extensions_mut().insert(LanExemptUnauthenticated);
        return next.run(req).await;
    }
    match crate::auth::verify_in(&entries, presented) {
        Some(id) => {
            if let Some(ip) = peer.map(|p| p.ip()) {
                state.auth.clear_auth_failures(ip);
            }
            state.auth.touch(&id);
            next.run(req).await
        }
        None => {
            let failures = peer
                .map(|p| state.auth.record_auth_failure(p.ip()))
                .unwrap_or(0);
            log_denied(
                peer,
                &path,
                if presented.is_empty() {
                    "no bearer token"
                } else {
                    "bearer token did not verify"
                },
                failures,
            );
            unauthorized("pair this client again (invalid or rotated token)")
        }
    }
}

/// High-risk irreversible or secret-exfiltration routes that are blocked with
/// `401 Unauthorized` from non-loopback private LAN peers while the Core is
/// still unpaired (`auth.configured == false`).
///
/// Read-only status/auth/console and one-shot `POST /api/v0/auth/provision`
/// stay reachable from private LAN so a household owner can pair a fresh NAS
/// Docker or speaker via the Web Console; public IPs are rejected outright with
/// `403 Forbidden` before reaching this check.
fn is_unpaired_lan_blocked_route(method: &axum::http::Method, path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    matches!(
        (method.as_str(), segs.as_slice()),
        ("GET" | "POST", ["api", "v0", "config", "export"])
            | ("POST", ["api", "v0", "config", "import"])
            | ("POST", ["api", "v0", "system", "factory-reset" | "restart"])
            | (_, ["api", "v0", "devices", _, "mic", ..])
            | ("POST", ["api", "v0", "devices", _, "wifi"])
            | (_, ["api", "v0", "release", ..])
            | (_, ["api", "v0", "firmware", ..])
            | ("POST", ["api", "v0", "auth", "rotate"])
            | ("PUT", ["api", "v0", "auth", "lan-exempt"])
            | ("POST", ["api", "v0", "fleet", "push"])
            | ("POST", ["api", "v0", "reflex", "deploy"])
    )
}

/// Transitional compatibility for pre-Phase-5 on-device `hm-release` binaries
/// before their one-shot USB flash (#231): only the exact management routes the
/// installer calls may proceed unauthenticated over loopback when no token is
/// presented; all other `/api/*` endpoints require a valid Bearer or local
/// release token even from `127.0.0.1` / `::1`.
fn is_legacy_loopback_release_route(method: &axum::http::Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", "/api/v0/status" | "/api/v0/mode" | "/api/v0/release")
            | (
                "POST",
                "/api/v0/release/snapshot" | "/api/v0/reflex/deploy" | "/api/v0/system/restart"
            )
    )
}

/// Marker inserted into request extensions when `require_bearer` admits an
/// unauthenticated private-LAN peer under `lan_gateway_exempt`. Handlers that
/// mix everyday toggles with privileged fields (such as `PUT /api/v0/mode/{dev}`)
/// inspect this marker to enforce field-level fail-closed authorization.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LanExemptUnauthenticated;

/// Enforce loopback speaker isolation for unauthenticated `lan_gateway_exempt`
/// LAN requests (`MG-P0-03`): a remote LAN/Android client without an admin
/// Bearer token may only operate on non-loopback devices, never on a local
/// loopback speaker such as `dot2-d`.
pub(super) fn reject_lan_exempt_loopback_device(
    state: &AppState,
    lan_exempt: Option<&axum::Extension<LanExemptUnauthenticated>>,
    device: &str,
) -> Result<(), (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    if lan_exempt.is_some() && state.hub.device_is_loopback_protected(device) {
        return Err((
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "admin bearer token required to access loopback speaker device"
            })),
        ));
    }
    Ok(())
}

/// Extract the owning device id from physical session namespaces:
/// - `dev:<device>`
/// - `dev:scope11:<device>:<epoch>`
/// - `dev:context12:<device>:<ctx_id>`
fn extract_physical_session_device(session_id: &str) -> Option<&str> {
    let rest = session_id.strip_prefix("dev:")?;
    if let Some(after_ns) = rest
        .strip_prefix("scope11:")
        .or_else(|| rest.strip_prefix("context12:"))
    {
        let (dev, _) = after_ns.rsplit_once(':')?;
        return (!dev.is_empty()).then_some(dev);
    }
    (!rest.is_empty()).then_some(rest)
}

/// Enforce loopback speaker isolation on session-scoped routes
/// (`GET /api/v0/session/{id}/messages`, `POST /api/v0/session/cancel`)
/// when called unauthenticated under `lan_gateway_exempt` (`MG-P0-03`).
pub(super) fn reject_lan_exempt_loopback_session(
    state: &AppState,
    lan_exempt: Option<&axum::Extension<LanExemptUnauthenticated>>,
    session_id: &str,
) -> Result<(), (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    if lan_exempt.is_none() {
        return Ok(());
    }
    let deny = || {
        Err((
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "admin bearer token required to access loopback speaker device"
            })),
        ))
    };
    if let Some(dev) = extract_physical_session_device(session_id) {
        if state.hub.device_is_loopback_protected(dev) {
            return deny();
        }
    }
    for dev in state.hub.loopback_protected_devices() {
        if session_id == format!("dev:{dev}")
            || session_id.starts_with(&format!("dev:scope11:{dev}:"))
            || session_id.starts_with(&format!("dev:context12:{dev}:"))
        {
            return deny();
        }
    }
    if let Some(owner) = state
        .sessions
        .device_for_context_session_any_epoch(session_id)
    {
        if state.hub.device_is_loopback_protected(&owner) {
            return deny();
        }
    }
    Ok(())
}

/// Option 1A (`lan_gateway_exempt`) strict allowlist of everyday household
/// read/chat/media/mode endpoints for private-LAN / Tailscale peers (#274).
///
/// Fail-closed by construction: all high-risk admin/security/OTA/firmware/
/// vault/config/credential/system/reflex/Wi-Fi routes are excluded and ALWAYS
/// require an admin Bearer token even when `lan_gateway_exempt = true`.
pub(crate) fn is_lan_exempt_everyday_route(method: &axum::http::Method, path: &str) -> bool {
    let clean_seg = |s: &str| !s.is_empty() && s != "." && s != ".." && !s.contains('/');
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match (method.as_str(), segs.as_slice()) {
        // Everyday read endpoints
        (
            "GET",
            ["api", "v0", "status"]
            | ["api", "v0", "mode"]
            | ["api", "v0", "devices"]
            | ["api", "v0", "device-meta"]
            | ["api", "v0", "fleet"]
            | ["api", "v0", "tasks"]
            | ["api", "v0", "reminders"]
            | ["api", "v0", "memories"]
            | ["api", "v0", "voice", "log"]
            | ["api", "v0", "voice", "events"],
        ) => true,
        ("GET", ["api", "v0", "devices", id, "conversation"]) if clean_seg(id) => true,
        ("GET", ["api", "v0", "session", id, "messages"]) if clean_seg(id) => true,
        // Everyday chat, voice session, playback & volume controls.
        // Note: `POST /api/v0/voice/announce` is intentionally EXCLUDED (external
        // callback that can open follow-up mic windows; always requires Bearer auth).
        (
            "POST",
            ["api", "v0", "chat"]
            | ["api", "v0", "session", "cancel"]
            | ["api", "v0", "session", "close"],
        ) => true,
        ("POST", ["api", "v0", "devices", id, "stop" | "volume"]) if clean_seg(id) => true,
        ("POST", ["api", "v0", "devices", id, "program", "pause" | "resume"]) if clean_seg(id) => {
            true
        }
        ("POST", ["api", "v0", "devices", id, "media", "control"]) if clean_seg(id) => true,
        // Everyday mode & mute toggles (privileged mode fields are additionally
        // rejected with 401 inside `mode_put` when `LanExemptUnauthenticated` is set)
        ("PUT", ["api", "v0", "mode", dev]) if clean_seg(dev) => true,
        ("PUT", ["api", "v0", "devices", id, "mute"]) if clean_seg(id) => true,
        _ => false,
    }
}

// --- M9: auth endpoints (bootstrap + rotation) --------------------------------

/// Open: the console needs to know whether this device was ever paired before
/// it can decide between "set up" and "enter token". When already paired,
/// unauthenticated callers only see `configured`/`corrupt` — token metadata
/// requires a valid bearer token.
async fn auth_status(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0);
    let non_loopback_ip = peer
        .map(|p| p.ip())
        .filter(|ip| !crate::net::is_loopback_ip(*ip));
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let core_id = state.auth.core_id();
    let lan_gateway_exempt = state.auth.lan_gateway_exempt();
    let voice_gateway_open = state.hub.voice_gateway_open();
    match state.auth.entries() {
        Ok(None) => Json(json!({
            "configured": false,
            "corrupt": false,
            "core_id": core_id,
            "lan_gateway_exempt": lan_gateway_exempt,
            "voice_gateway_open": voice_gateway_open,
            "tokens": [],
            "loopback_trusted": true,
        }))
        .into_response(),
        Ok(Some(list)) => {
            if let Some(ip) = non_loopback_ip {
                if let Some(retry_after) = state.auth.is_rate_limited(ip) {
                    return too_many_requests(retry_after);
                }
            }
            if presented.is_empty() {
                return Json(json!({
                    "configured": true,
                    "corrupt": false,
                    "core_id": core_id,
                    "lan_gateway_exempt": lan_gateway_exempt,
                    "voice_gateway_open": voice_gateway_open,
                }))
                .into_response();
            }
            match crate::auth::verify_in(&list, presented) {
                None => {
                    if let Some(ip) = non_loopback_ip {
                        let failures = state.auth.record_auth_failure(ip);
                        log_denied(
                            peer,
                            "/api/v0/auth",
                            "bearer token did not verify",
                            failures,
                        );
                        if let Some(retry_after) = state.auth.is_rate_limited(ip) {
                            return too_many_requests(retry_after);
                        }
                    }
                    Json(json!({
                        "configured": true,
                        "corrupt": false,
                        "core_id": core_id,
                        "lan_gateway_exempt": lan_gateway_exempt,
                        "voice_gateway_open": voice_gateway_open,
                    }))
                    .into_response()
                }
                Some(id) => {
                    if let Some(ip) = non_loopback_ip {
                        state.auth.clear_auth_failures(ip);
                    }
                    state.auth.touch(&id);
                    Json(json!({
                        "configured": true,
                        "corrupt": false,
                        "core_id": core_id,
                        "lan_gateway_exempt": lan_gateway_exempt,
                        "voice_gateway_open": voice_gateway_open,
                        "tokens": list.iter().map(|e| json!({
                            "id": e.id,
                            "label": e.label,
                            "created_ms": e.created_ms,
                            "last_used_ms": e.last_used_ms,
                        })).collect::<Vec<_>>(),
                        "loopback_trusted": true,
                    }))
                    .into_response()
                }
            }
        }
        Err(_) => Json(json!({
            "configured": true,
            "corrupt": true,
            "core_id": core_id,
            "lan_gateway_exempt": lan_gateway_exempt,
            "voice_gateway_open": voice_gateway_open,
        }))
        .into_response(),
    }
}

/// Open only while the device was never paired. The plaintext token is
/// returned exactly once; the core keeps only its SHA-256.
async fn auth_provision(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.auth.provision("family") {
        Ok(token) => {
            tracing::info!("auth: household token provisioned");
            Ok(Json(json!({
                "token": token,
                "created_ms": crate::id::now_ms(),
                "note": "shown once; store it where the other household credentials live",
            })))
        }
        Err(crate::auth::AuthError::AlreadyConfigured) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "already paired",
                "detail": "use POST /api/v0/auth/rotate with the current token",
            })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

#[derive(serde::Deserialize)]
struct LanExemptBody {
    lan_gateway_exempt: bool,
}

/// Authenticated (strict admin Bearer token required even when LAN exemption is
/// enabled): toggle Option 1A private-LAN exemption for `:8932` voice gateway
/// and everyday `:8930` read/chat/media/mode APIs.
async fn auth_lan_exempt_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LanExemptBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.auth.set_lan_gateway_exempt(body.lan_gateway_exempt) {
        Ok(applied) => {
            let summary = format!("lan_gateway_exempt -> {applied}");
            let payload = json!({ "lan_gateway_exempt": applied }).to_string();
            let _ = state.store.with(|c| {
                c.execute(
                    "INSERT INTO config_history (ts_ms, actor, scope, summary, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![crate::id::now_ms() as i64, "console", "auth", summary, payload],
                )?;
                Ok(())
            });
            tracing::info!(
                lan_gateway_exempt = applied,
                "auth: updated Option 1A private-LAN exemption"
            );
            Ok(Json(json!({
                "lan_gateway_exempt": applied,
                "core_id": state.auth.core_id(),
            })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

/// Authenticated (middleware): replaces every token with one fresh token.
async fn auth_rotate(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.auth.rotate("family") {
        Ok(token) => {
            tracing::info!("auth: household token rotated; previous tokens revoked");
            Ok(Json(json!({
                "token": token,
                "created_ms": crate::id::now_ms(),
                "note": "all previously paired clients must pair again",
            })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

// --- built-in console (M4: single-port NAS deployment) ----------------------------

/// Serve the built console (web/dist) from the core itself. Unknown /api
/// paths stay honest 404 JSON; every other path resolves inside web_dir
/// (SPA: unknown paths serve index.html). No new dependency — a small
/// read-only resolver with a traversal guard.
async fn console_fallback(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if uri.path().starts_with("/api/") || state.web_dir.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response();
    }
    let dir = state.web_dir.as_ref().unwrap();
    match resolve_console_path(dir, uri.path()) {
        Some(path) => match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let mime = console_mime(&path);
                // 2026-11-14: a browser that kept an old index.html kept
                // running an old bundle against a newer API (269 `401 no
                // bearer token` polls after the auth release). The entry
                // document must revalidate; the hashed assets never change
                // under the same name.
                let cache = console_cache_control(&path);
                (
                    [
                        (axum::http::header::CONTENT_TYPE, mime),
                        (axum::http::header::CACHE_CONTROL, cache),
                    ],
                    bytes,
                )
                    .into_response()
            }
            Err(_) => {
                (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
            }
        },
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response(),
    }
}

/// Map a request path to a file under `dir`: "/" → index.html, real files
/// resolve directly, extensionless SPA routes fall back to index.html.
/// Returns None on traversal attempts (an existing path that escapes the
/// base) or missing assets/files — the caller turns that into a 404.
fn resolve_console_path(dir: &std::path::Path, path: &str) -> Option<PathBuf> {
    let base = dir.canonicalize().ok()?;
    let index = || base.join("index.html");
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let candidate = base.join(rel);
    let spa_route =
        !rel.starts_with("assets/") && rel != "assets" && candidate.extension().is_none();
    let canon = match candidate.canonicalize() {
        Ok(c) => c,
        Err(_) => return (spa_route && index().is_file()).then_some(index()),
    };
    if !canon.starts_with(&base) {
        return None; // traversal or symlink escape
    }
    if canon.is_file() {
        return Some(canon);
    }
    (spa_route && index().is_file()).then_some(index())
}

fn console_cache_control(path: &std::path::Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let in_assets = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n == "assets")
        .unwrap_or(false);
    if in_assets && name != "index.html" {
        // Vite writes a content hash into every asset name.
        "public, max-age=31536000, immutable"
    } else {
        // The entry document (and SPA fallbacks that resolve to it) must be
        // revalidated so a release can never leave a stale bundle running.
        "no-cache"
    }
}

fn console_mime(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

// --- devices & mode (M2) ------------------------------------------------------

pub(super) fn bad_request(e: impl ToString) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": e.to_string() })),
    )
}

#[cfg(test)]
mod console_tests {
    use super::resolve_console_path;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        // Per-call suffix: both console tests run in parallel threads of the
        // same process, and a pid-only path let one test's remove_dir_all
        // delete the other's fixture mid-run (NotFound flake observed during
        // the 2026-11-10 CPU batch).
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hm-console-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<html>console</html>").unwrap();
        std::fs::write(dir.join("assets/app.js"), "console.log(1)").unwrap();
        dir
    }

    #[test]
    fn root_and_real_files_resolve() {
        let dir = fixture();
        assert!(resolve_console_path(&dir, "/")
            .unwrap()
            .ends_with("index.html"));
        assert!(resolve_console_path(&dir, "/assets/app.js")
            .unwrap()
            .ends_with("app.js"));
        // SPA route without a file behind it falls back to index.html.
        assert!(resolve_console_path(&dir, "/diagnostics")
            .unwrap()
            .ends_with("index.html"));
    }

    #[test]
    fn missing_assets_return_not_found_instead_of_html() {
        let dir = fixture();
        assert!(resolve_console_path(&dir, "/assets/x.js").is_none());
        assert!(resolve_console_path(&dir, "/assets/nested/missing").is_none());
        assert!(resolve_console_path(&dir, "/missing.css").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_frontend_route_still_serves_index() {
        let dir = fixture();
        let path = resolve_console_path(&dir, "/unknown/frontend/route").unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "<html>console</html>"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn missing_asset_is_404_and_spa_route_is_html() {
        let dir = fixture();
        let state = super::AppState::with_overrides(
            std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap()),
            dir.clone(),
            crate::tool::mocks::MockInject::default(),
            Some(std::sync::Arc::new(crate::llm::fake::FakeLlm::new(vec![]))),
            None,
            None,
            Some(dir.clone()),
        );
        let missing = super::console_fallback(
            axum::extract::State(state.clone()),
            "/assets/x.js".parse().unwrap(),
        )
        .await;
        assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
        let route = super::console_fallback(
            axum::extract::State(state),
            "/unknown/frontend/route".parse().unwrap(),
        )
        .await;
        assert_eq!(route.status(), axum::http::StatusCode::OK);
        assert_eq!(
            route.headers()[axum::http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let body = axum::body::to_bytes(route.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"<html>console</html>");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn traversal_attempts_resolve_to_none() {
        let dir = fixture();
        let secret = std::env::temp_dir().join(format!("hm-console-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&secret);
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("secret.txt"), "s").unwrap();
        // ../<secret>/secret.txt must not resolve (canonicalize escapes base).
        let rel = format!(
            "../{}/secret.txt",
            secret.file_name().unwrap().to_str().unwrap()
        );
        assert!(
            resolve_console_path(&dir, &format!("/{rel}")).is_none()
                || !resolve_console_path(&dir, &format!("/{rel}"))
                    .unwrap()
                    .ends_with("secret.txt")
        );
    }

    #[tokio::test]
    async fn auth_status_rate_limits_repeated_bad_bearer_tokens() {
        let dir = fixture();
        let state = super::AppState::with_overrides(
            std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap()),
            dir.clone(),
            crate::tool::mocks::MockInject::default(),
            Some(std::sync::Arc::new(crate::llm::fake::FakeLlm::new(vec![]))),
            None,
            None,
            None,
        );
        let _token = state.auth.provision("family").unwrap();
        let peer: std::net::SocketAddr = "192.168.1.99:43210".parse().unwrap();
        let mut saw_429 = false;
        for _ in 0..25 {
            let mut req = axum::http::Request::builder()
                .uri("/api/v0/auth")
                .header(axum::http::header::AUTHORIZATION, "Bearer hm1_wrong_token")
                .body(axum::body::Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
            let resp = super::auth_status(axum::extract::State(state.clone()), req).await;
            if resp.status() == axum::http::StatusCode::TOO_MANY_REQUESTS {
                saw_429 = true;
                break;
            }
        }
        assert!(
            saw_429,
            "repeated bad tokens on /api/v0/auth must trigger 429"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn config_export_secrets_requires_post_and_confirm_header() {
        let dir = fixture();
        let state = super::AppState::with_overrides(
            std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap()),
            dir.clone(),
            crate::tool::mocks::MockInject::default(),
            Some(std::sync::Arc::new(crate::llm::fake::FakeLlm::new(vec![]))),
            None,
            None,
            None,
        );
        let q = serde_json::from_value(serde_json::json!({ "secrets": "1" })).unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            crate::api::system::CONFIRM_SECRETS_HEADER,
            axum::http::HeaderValue::from_static(crate::api::system::CONFIRM_SECRETS_VALUE),
        );
        // GET with secrets must be rejected (405 Method Not Allowed) even with header.
        let err = super::config_export(
            axum::http::Method::GET,
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::extract::Query(q),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::METHOD_NOT_ALLOWED);

        // POST without confirm header must be rejected (403 Forbidden).
        let q2 = serde_json::from_value(serde_json::json!({ "secrets": "1" })).unwrap();
        let err = super::config_export(
            axum::http::Method::POST,
            axum::extract::State(state.clone()),
            axum::http::HeaderMap::new(),
            axum::extract::Query(q2),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::FORBIDDEN);

        // POST with confirm header succeeds.
        let q3 = serde_json::from_value(serde_json::json!({ "secrets": "1" })).unwrap();
        let ok = super::config_export(
            axum::http::Method::POST,
            axum::extract::State(state),
            headers,
            axum::extract::Query(q3),
        )
        .await
        .unwrap();
        assert!(ok.0.get("secrets").is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
