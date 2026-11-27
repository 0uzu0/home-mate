//! Read the same per-operation factories used by chat and voice. This is
//! construction/readback evidence, never a remote-health or publication claim.
use super::*;
use crate::{config::ConfigService, speech};
use serde_json::json;
use std::sync::Arc;

impl ConfigService {
    pub fn runtime_tts(&self) -> std::result::Result<Arc<dyn speech::TtsClient>, String> {
        self.runtime_snapshot()?.tts()
    }
    pub fn runtime_asr(&self) -> std::result::Result<Arc<dyn speech::AsrClient>, String> {
        self.runtime_snapshot()?.asr()
    }
}

pub(super) fn capture(
    store: Arc<crate::store::Store>,
    voice: Option<&crate::voice::VoicePipeline>,
    open_sockets: Option<&super::open_sockets::Provider>,
) -> Result<serde_json::Value> {
    let allow_loopback = store.allows_loopback_public();
    let config = ConfigService::new(store.clone());
    let runtime = config.runtime_snapshot().map_err(DeliveryError::internal)?;
    let cfg = &runtime.config;
    let mut fields = Fields::new();
    super::service::flatten("services", &serde_json::to_value(&cfg)?, &mut fields);
    let persona = config
        .prompt_preferences()
        .map_err(DeliveryError::internal)?
        .agent_md;
    fields.insert("persona.agent_md.content".into(), json!(persona));
    let mut states = serde_json::Map::new();
    let mut cue_readiness = serde_json::Value::Null;
    if let Some(voice) = voice {
        let current = voice.settings_snapshot();
        let projection = super::mode::projection(&current);
        if super::mode::resolve_projection(&current, &projection).is_ok() {
            let mut observed = Fields::new();
            super::service::flatten("mode", &projection, &mut observed);
            for path in observed.keys() {
                states.insert(path.clone(), json!("core_readback_verified"));
            }
            fields.extend(observed);
        }
        cue_readiness = voice.cue_readiness();
    }
    use super::discovery::Block;
    for (block, value) in [
        (
            Block::Music,
            config.music().ok().map(serde_json::to_value).transpose()?,
        ),
        (
            Block::Home,
            config.home().ok().map(serde_json::to_value).transpose()?,
        ),
        (
            Block::OpenApi,
            config
                .openapi()
                .ok()
                .map(serde_json::to_value)
                .transpose()?,
        ),
        (
            Block::News,
            config.news().ok().map(serde_json::to_value).transpose()?,
        ),
        (
            Block::OpenSockets,
            open_sockets
                .and_then(|provider| provider.effective(&store, None))
                .map(serde_json::to_value)
                .transpose()?,
        ),
        (
            Block::Search,
            config.search().ok().map(serde_json::to_value).transpose()?,
        ),
    ] {
        if let Some(value) =
            value.filter(|value| block.validate(value.clone(), allow_loopback).is_ok())
        {
            let mut observed = Fields::new();
            super::service::flatten(block.key(), &value, &mut observed);
            for path in observed.keys() {
                states.insert(path.clone(), json!("core_readback_verified"));
            }
            fields.extend(observed);
        }
    }
    if let Some(weather) = config.weather_snapshot().ok().filter(|weather| {
        crate::config::WeatherConfig::endpoint_is_portable(&weather.config.endpoint)
    }) {
        let mut weather_fields = Fields::new();
        super::service::flatten(
            "weather",
            &serde_json::to_value(weather.config)?,
            &mut weather_fields,
        );
        for path in weather_fields.keys() {
            let state = if path == "weather.api_key_env" {
                "binding_name_readback"
            } else {
                "core_readback_verified"
            };
            states.insert(path.clone(), json!(state));
        }
        fields.extend(weather_fields);
    }
    states.insert(
        "persona.agent_md.content".into(),
        json!("core_readback_verified"),
    );
    if let Ok(eq) = config.audio_eq() {
        let mut eq_fields = Fields::new();
        super::service::flatten("audio_eq", &serde_json::to_value(&eq)?, &mut eq_fields);
        for path in eq_fields.keys() {
            states.insert(path.clone(), json!("core_readback_verified"));
        }
        fields.extend(eq_fields);
    }
    if let Ok(rfx) = config.reflex() {
        let mut rfx_fields = Fields::new();
        super::service::flatten("reflex", &serde_json::to_value(&rfx)?, &mut rfx_fields);
        for path in rfx_fields.keys() {
            states.insert(path.clone(), json!("core_readback_verified"));
        }
        fields.extend(rfx_fields);
    }
    if let Ok(leds) = config.device_led_configs() {
        let mut led_fields = Fields::new();
        super::service::flatten(
            "device_led_configs",
            &serde_json::to_value(&leds)?,
            &mut led_fields,
        );
        for path in led_fields.keys() {
            states.insert(path.clone(), json!("core_readback_verified"));
        }
        fields.extend(led_fields);
    }
    if let Ok(acoustics) = config.unit_acoustic_profiles() {
        let mut ac_fields = Fields::new();
        super::service::flatten(
            "unit_acoustic_profiles",
            &serde_json::to_value(&acoustics)?,
            &mut ac_fields,
        );
        for path in ac_fields.keys() {
            states.insert(path.clone(), json!("core_readback_verified"));
        }
        fields.extend(ac_fields);
    }
    let node_val = store
        .config_get("node")
        .map_err(DeliveryError::internal)?
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| json!({"wake": {"phrases": []}, "led": {"timezone": "Asia/Shanghai"}}));
    let mut node_fields = Fields::new();
    super::service::flatten("node", &node_val, &mut node_fields);
    for path in node_fields.keys() {
        states.insert(path.clone(), json!("core_readback_verified"));
    }
    fields.extend(node_fields);
    for (name, service) in [
        ("llm", &cfg.llm),
        ("llm_fallback", &cfg.llm_fallback),
        ("asr", &cfg.asr),
        ("tts", &cfg.tts),
    ] {
        // Error details can contain provider-supplied data. Evidence reports
        // a stable class only; credentials never enter this document.
        let constructed = match name {
            "llm" | "llm_fallback" => runtime.llm_client(name).map(|_| ()),
            "asr" => runtime.asr().map(|_| ()),
            _ => runtime.tts().map(|_| ()),
        };
        for field in [
            "provider",
            "base_url",
            "model",
            "api_key_env",
            "thinking",
            "voice",
            "language",
        ] {
            let consumed = match name {
                "llm" | "llm_fallback" => !["voice", "language"].contains(&field),
                "asr" => !["voice", "thinking"].contains(&field),
                _ => {
                    !["thinking", "language"].contains(&field)
                        && (service.provider != "mock" || field == "provider")
                }
            };
            let state = if !service.is_configured() || (name == "asr" && service.provider == "mock")
            {
                "inactive"
            } else if !consumed {
                "not_consumed"
            } else if constructed.is_err() {
                "reader_error"
            } else if field == "api_key_env" {
                "binding_name_readback"
            } else {
                "core_readback_verified"
            };
            states.insert(format!("services.{name}.{field}"), json!(state));
        }
    }
    Ok(
        json!({"fields":fields,"field_states":states,"cue_readiness":cue_readiness,"scope":"next_operation_native_factories_and_prompt","remote_health":"not_probed","publication_isolation":"not_implemented"}),
    )
}
