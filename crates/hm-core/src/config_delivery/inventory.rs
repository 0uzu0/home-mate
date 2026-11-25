//! Common path catalogue. Registered does not mean supported by this slice.
use super::{DeliveryError, Purpose, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Bounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FieldDescriptor {
    pub path: String,
    pub kind: String,
    /// Serde representation only; the owning validator may narrow values.
    /// Descriptive metadata never enables package application.
    pub nullable: bool,
    pub array_item_kind: Option<String>,
    pub scope: String,
    pub supported: bool,
    pub purposes: Vec<String>,
    pub unit: String,
    pub bounds: Option<Bounds>,
    pub default_source: String,
    pub validator: String,
    pub secret_policy: String,
    pub apply: String,
    pub rollback: String,
    pub clear_behavior: String,
}

pub const BINDINGS: &[&str] = &[
    "llm.api_key",
    "llm_fallback.api_key",
    "asr.api_key",
    "tts.api_key",
    "music.library.secret",
    "music.online.secret",
    "weather.api_key",
    "home.token",
    "openapi.token",
    "search.api_key",
    "shared_memory_token",
];

pub fn fields() -> Vec<FieldDescriptor> {
    let mut out = Vec::new();
    let mut add = |paths: Vec<String>, kind: &str, scope: &str, supported: bool, owner: &str| {
        for path in paths {
            out.push(FieldDescriptor {
                path,
                kind: kind.into(),
                nullable: false,
                array_item_kind: None,
                scope: scope.into(),
                supported,
                purposes: if !supported {
                    vec![]
                } else if scope == "node" || scope == "unit" {
                    vec!["node_config".into(), "same_node_restore".into()]
                } else {
                    vec![
                        "family_policy".into(),
                        "node_config".into(),
                        "same_node_restore".into(),
                    ]
                },
                unit: if kind == "boolean" {
                    "boolean"
                } else {
                    "schema-defined"
                }
                .into(),
                bounds: None,
                default_source: owner.into(),
                validator: owner.into(),
                secret_policy: "no_secret_values; preserve_or_require_binding".into(),
                apply: if supported {
                    "saved_pending; next-operation/runtime projection not certified"
                } else {
                    "unsupported_in_slice"
                }
                .into(),
                rollback: "not_implemented_in_slice".into(),
                clear_behavior: if supported {
                    "remove_layer_override"
                } else {
                    "unsupported"
                }
                .into(),
            });
        }
    };
    fn paths(prefix: &str, names: &str) -> Vec<String> {
        names
            .split_whitespace()
            .map(|n| format!("{prefix}.{n}"))
            .collect()
    }
    for service in ["llm", "llm_fallback", "asr", "tts"] {
        let prefix = format!("services.{service}");
        add(
            paths(&prefix, "provider base_url model thinking voice"),
            "string",
            "family_or_node",
            true,
            "config::ServiceConfig/speech provider factory",
        );
        add(
            paths(&prefix, "api_key_env"),
            "string",
            "node",
            true,
            "config::ServiceConfig",
        );
    }
    add(
        paths("shared_memory", "enabled upload_enabled"),
        "boolean",
        "family_or_node",
        true,
        "config::SharedMemoryConfig::validate",
    );
    add(
        paths("shared_memory", "nas_base_url"),
        "string",
        "family_or_node",
        true,
        "config::SharedMemoryConfig::validate",
    );
    add(
        vec!["persona.agent_md.content".into()],
        "string",
        "family_or_node",
        true,
        "agent_md::MAX_AGENT_MD_CHARS",
    );
    for (names, kind, scope) in [
        ("enabled", "boolean", "family_or_node"),
        (
            "provider endpoint default_alias",
            "string",
            "family_or_node",
        ),
        ("max_days", "number", "family_or_node"),
        ("places", "array", "family_or_node"),
        ("api_key_env", "string", "node"),
    ] {
        add(
            paths("weather", names),
            kind,
            scope,
            true,
            "config::WeatherConfig",
        );
    }
    for (prefix, names, kind, owner) in [
        (
            "music",
            "enabled prefer_local",
            "boolean",
            "config::MusicConfig",
        ),
        ("music", "library online", "object", "config::MusicConfig"),
        ("openapi", "enabled", "boolean", "config::OpenApiConfig"),
        (
            "openapi",
            "spec_url base_url auth_header",
            "string",
            "config::OpenApiConfig",
        ),
        (
            "openapi",
            "timeout_ms retries",
            "integer",
            "config::OpenApiConfig",
        ),
        ("openapi", "operations", "array", "config::OpenApiConfig"),
        (
            "home",
            "backend entity_source base_url",
            "string",
            "config::HomeConfig",
        ),
        (
            "home",
            "timeout_ms retries",
            "integer",
            "config::HomeConfig",
        ),
        ("home", "entities", "array", "config::HomeConfig"),
        ("news", "enabled", "boolean", "config::NewsConfig"),
        ("news", "default_alias", "string", "config::NewsConfig"),
        ("news", "timeout_ms", "integer", "config::NewsConfig"),
        ("news", "max_sources", "integer", "config::NewsConfig"),
        ("news", "sources", "array", "config::NewsConfig"),
        ("search", "enabled", "boolean", "config::SearchConfig"),
        ("search", "base_url", "string", "config::SearchConfig"),
        (
            "search",
            "count timeout_ms",
            "integer",
            "config::SearchConfig",
        ),
    ] {
        add(paths(prefix, names), kind, "family_or_node", true, owner);
    }
    for (names, kind, scope) in [
        ("assistant_name persona_prompt", "string", "family_or_node"),
        ("system_prompt_override", "string", "node"),
        (
            "follow_window_enabled follow_window_tone",
            "boolean",
            "family_or_node",
        ),
        (
            "follow_window_ms context_window_ms followup_credits",
            "integer",
            "family_or_node",
        ),
    ] {
        add(
            paths("mode", names),
            kind,
            scope,
            true,
            "voice::VoiceSettings",
        );
    }
    for (prefix, names, kind) in [
        ("mode", "retreat_intent", "boolean"),
        ("mode", "retreat_words", "array"),
        ("mode", "retreat_ack window_close_ack truncation_notice", "string"),
        ("mode.wake_ack", "enabled", "boolean"),
        ("mode.wake_ack", "phrases", "array"),
        ("mode.filler", "delay_ms tool_delay_ms", "integer"),
        ("mode.filler", "mode", "string"),
        ("mode.filler", "thinking search device story", "array"),
        ("mode.stop_intents", "enabled", "boolean"),
        ("mode.stop_intents", "stop_words continue_words", "array"),
        ("mode.spoken", "max_sentences max_chars detail_max_sentences detail_max_chars backstop_max_sentences backstop_max_chars", "integer"),
        ("mode.spoken", "continue_hint resume_lead", "string"),
    ] {
        add(paths(prefix, names), kind, "family_or_node", true, "voice::VoiceSettings");
    }
    for (prefix, names, kind) in [
        (
            "music",
            "library.endpoint library.user online.endpoint online.user",
            "string",
        ),
        (
            "weather.places[]",
            "alias name timezone city_code",
            "string",
        ),
        ("weather.places[]", "latitude longitude", "number"),
        ("home.entities[]", "name entity", "string"),
        ("news.sources[]", "alias url kind", "string"),
        ("news.sources[]", "max_items", "number"),
        (
            "openapi.operations[]",
            "id method path summary effect_level",
            "string",
        ),
        (
            "openapi.operations[].params[]",
            "name location kind",
            "string",
        ),
        (
            "openapi.operations[].params[]",
            "required sensitive",
            "boolean",
        ),
        ("openapi.operations[].params[]", "enum_values", "array"),
        (
            "openapi.operations[].verification",
            "type op_id json_path",
            "string",
        ),
        ("openapi.operations[].verification", "expected", "object"),
        ("mode", "proactive_speech", "boolean"),
        ("mode", "wake_arbitration", "string"),
        ("mode.spoken", "detail_words long_form_words", "array"),
        ("devices.*", "name room", "string"),
    ] {
        add(
            paths(prefix, names),
            kind,
            "family_or_node",
            false,
            "existing block validator; C3/C4 integration pending",
        );
    }
    for (prefix, names, kind) in [
        ("reflex.words[]", "text tokens intent", "string"),
        ("reflex.words[]", "enabled", "boolean"),
    ] {
        add(
            paths(prefix, names),
            kind,
            "family_or_node",
            true,
            "config::ReflexConfig",
        );
    }
    for (prefix, names, kind) in [
        ("mode", "wake_words", "array"),
        ("mode", "duplex_requested", "object"),
        ("node.wake", "phrases", "array"),
        ("audio_eq", "curve", "string"),
        ("audio_eq.bands[]", "freq gain_db q", "number"),
        (
            "device_led_configs.*",
            "night_enabled orientation_reverse",
            "boolean",
        ),
        (
            "device_led_configs.*",
            "night_start_minute night_end_minute night_brightness_pct orientation_rotation",
            "number",
        ),
        ("node.led", "timezone", "string"),
    ] {
        add(
            paths(prefix, names),
            kind,
            "node",
            true,
            "gateway::config_apply_v2",
        );
    }
    for (prefix, names, kind) in [
        ("mode", "keyword_interrupt barge_in", "boolean"),
        ("mode", "barge_protect_ms", "number"),
        ("privacy", "idle_asr_enabled", "boolean"),
        ("open_sockets[]", "name service description endpoint_url method auth_header default_subject default_action", "string"),
        ("open_sockets[]", "effect", "string"),
        ("open_sockets[]", "parameters", "object"),
        ("open_sockets[]", "enabled", "boolean"),
        ("open_sockets[]", "timeout_ms", "number"),
    ] { add(paths(prefix, names), kind, "node", false, "target-specific; unsupported in slice"); }
    add(
        paths("mode", "kws_threshold kws_score"),
        "number",
        "unit",
        true,
        "gateway::config_apply_v2; exact-unit gated",
    );
    add(
        paths(
            "mode",
            "media_trim_db tail_blind_ms vad.start_db vad.end_db",
        ),
        "number",
        "unit",
        false,
        "explicit calibration authority only",
    );
    add(
        paths(
            "unit_acoustic_profiles.*",
            "volume_cap mix_gain vad_threshold onset_threshold_dbfs mic_gain_offset_db",
        ),
        "number",
        "unit",
        true,
        "config::UnitAcousticProfile; exact-unit gated",
    );
    add(
        paths("unit_acoustic_profiles.*", "mic_calibrated"),
        "boolean",
        "unit",
        false,
        "config::UnitAcousticProfile; measured provenance; never package-writable",
    );
    // Containers and native aliases are registered too. Their presence is
    // descriptive; individual supported leaves retain their existing gate.
    for (prefix, names, kind, scope, supported) in [
        (
            "services",
            "llm llm_fallback asr tts",
            "object",
            "family_or_node",
            false,
        ),
        (
            "openapi.operations[]",
            "params",
            "array",
            "family_or_node",
            false,
        ),
        (
            "openapi.operations[]",
            "verification",
            "object",
            "family_or_node",
            false,
        ),
        (
            "mode",
            "wake_ack stop_intents filler spoken",
            "object",
            "family_or_node",
            false,
        ),
        (
            "mode",
            "asr_wake_fallback proactive_asr_wake",
            "boolean",
            "node",
            false,
        ),
        ("mode", "vad", "object", "unit", false),
        ("mode", "vad_by_device", "object", "unit", false),
        ("mode.vad_by_device", "*", "object", "unit", false),
        (
            "mode.vad_by_device.*",
            "start_db end_db",
            "number",
            "unit",
            false,
        ),
        ("mode", "media_trim_db_by_device", "object", "unit", false),
        ("mode.media_trim_db_by_device", "*", "number", "unit", false),
        ("mode", "device_overrides", "object", "unit", false),
        ("mode.device_overrides", "*", "object", "unit", false),
        (
            "mode.device_overrides.*",
            "kws_threshold kws_score tail_blind_ms barge_protect_ms follow_window_ms context_window_ms followup_credits",
            "number",
            "unit",
            false,
        ),
        (
            "mode.device_overrides.*",
            "barge_in keyword_interrupt proactive_speech follow_window_enabled follow_window_tone retreat_intent asr_wake_fallback proactive_asr_wake",
            "boolean",
            "unit",
            false,
        ),
        (
            "mode.device_overrides.*",
            "wake_words retreat_words",
            "array",
            "unit",
            false,
        ),
        ("mode.device_overrides.*", "wake_ack", "object", "unit", false),
        (
            "mode.device_overrides.*.wake_ack",
            "enabled resynth",
            "boolean",
            "unit",
            false,
        ),
        (
            "mode.device_overrides.*.wake_ack",
            "phrases",
            "array",
            "unit",
            false,
        ),
        (
            "mode.device_overrides.*",
            "wake_arbitration",
            "string",
            "unit",
            false,
        ),
        ("mode.duplex_requested", "*", "string", "node", true),
        ("mode.wake_ack", "resynth", "boolean", "runtime", false),
        ("mode.filler", "resynth", "boolean", "runtime", false),
        ("reflex", "words", "array", "family_or_node", true),
        ("audio_eq", "bands", "array", "node", true),
        ("fleet", "self_id", "string", "boot", false),
        ("fleet", "nodes", "array", "boot", false),
        ("fleet.nodes[]", "id base_url", "string", "boot", false),
        (
            "fleet.nodes[]",
            "added_ms last_ok_ms",
            "integer",
            "runtime",
            false,
        ),
        ("fleet.nodes[]", "last_error", "string", "runtime", false),
        ("db", "maintenance", "object", "runtime", false),
    ] {
        add(
            paths(prefix, names),
            kind,
            scope,
            supported,
            if supported {
                "gateway::config_apply_v2"
            } else {
                "native serde shape; separate owner/validator; application unsupported"
            },
        );
    }
    for prefix in [
        "story",
        "joke",
        "radio",
        "learning",
        "services.story",
        "services.joke",
        "services.radio",
        "services.learning",
    ] {
        for (names, kind) in [
            ("enabled", "boolean"),
            ("url", "string"),
            ("timeout_ms", "number"),
        ] {
            add(
                paths(prefix, names),
                kind,
                "family_or_node",
                false,
                "config::PersonalEndpointConfig; separate owner/validator; application unsupported",
            );
        }
    }
    add(
        vec!["open_sockets".into()],
        "array",
        "node",
        true,
        "tool::open_socket::OpenSocketContract",
    );
    for (name, scope) in [
        ("learning", "data"),
        ("user_md", "data"),
        ("working_memory", "data"),
        ("fleet", "boot"),
        ("secrets", "secret"),
        ("runtime", "runtime"),
    ] {
        add(
            vec![name.into()],
            "object",
            scope,
            false,
            "separate authority; unsupported in slice",
        );
    }
    for field in &mut out {
        field.nullable = matches!(
            field.path.as_str(),
            "music.library"
                | "music.online"
                | "mode.system_prompt_override"
                | "mode.followup_credits"
                | "mode.vad"
                | "mode.wake_ack.resynth"
                | "mode.device_overrides.*.wake_ack.resynth"
                | "mode.filler.resynth"
                | "openapi.operations[].verification"
                | "unit_acoustic_profiles.*.onset_threshold_dbfs"
        ) || (field.path.starts_with("mode.device_overrides.*.")
            && !field.path.starts_with("mode.device_overrides.*.wake_ack."));
        if field.path == "openapi.operations[].verification.expected"
            || field.path.ends_with(".parameters")
        {
            field.kind = "json".into();
            field.nullable = true;
        }
        if field.kind == "number"
            && !(field.path.starts_with("audio_eq.bands[]")
                || field.path.starts_with("unit_acoustic_profiles.")
                || matches!(
                    field.path.as_str(),
                    "weather.places[].latitude"
                        | "weather.places[].longitude"
                        | "mode.kws_threshold"
                        | "mode.kws_score"
                        | "mode.device_overrides.*.kws_threshold"
                        | "mode.device_overrides.*.kws_score"
                        | "mode.media_trim_db"
                        | "mode.media_trim_db_by_device.*"
                        | "mode.vad.start_db"
                        | "mode.vad.end_db"
                        | "mode.vad_by_device.*.start_db"
                        | "mode.vad_by_device.*.end_db"
                ))
        {
            field.kind = "integer".into();
        }
        if field.kind == "array" {
            field.array_item_kind = Some(
                if matches!(
                    field.path.as_str(),
                    "weather.places"
                        | "home.entities"
                        | "news.sources"
                        | "openapi.operations"
                        | "openapi.operations[].params"
                        | "reflex.words"
                        | "audio_eq.bands"
                        | "open_sockets"
                        | "fleet.nodes"
                ) {
                    "object"
                } else {
                    "string"
                }
                .into(),
            );
        }
        if field.supported
            && (field.path.starts_with("reflex.")
                || field.path.starts_with("node.")
                || field.path.starts_with("audio_eq.")
                || field.path.starts_with("device_led_configs.")
                || field.path.starts_with("unit_acoustic_profiles.")
                || matches!(
                    field.path.as_str(),
                    "mode.wake_words" | "mode.kws_threshold" | "mode.kws_score"
                )
                || field.path.starts_with("mode.duplex_requested"))
        {
            field.apply = "saved_pending; gateway_config_apply_v2; gateway_receipt_verified".into();
            field.rollback = "reviewed_plan; gateway_compensation_on_failure".into();
        } else if field.supported
            && (field.path.starts_with("services.")
                || matches!(
                    field.path.as_str(),
                    "persona.agent_md.content" | "open_sockets"
                )
                || [
                    "weather.", "home.", "music.", "news.", "search.", "openapi.", "mode.",
                ]
                .iter()
                .any(|prefix| field.path.starts_with(prefix)))
        {
            field.apply =
                "saved_pending; supervised_core_publication; separate_native_readback".into();
            field.rollback = "reviewed_plan; verified_before_values_only".into();
        }
        if field.path.starts_with("shared_memory.") {
            field.apply = "saved_pending; runtime_dependency:#107".into();
        }
        enrich_descriptor(field);
    }
    out
}

fn enrich_descriptor(field: &mut FieldDescriptor) {
    let p = field.path.as_str();
    field.unit = if field.kind == "boolean" {
        "boolean"
    } else if p.ends_with("_ms") {
        if p.contains("fleet.nodes") {
            "epoch_ms"
        } else {
            "ms"
        }
    } else if p == "weather.max_days" {
        "days"
    } else if p.ends_with(".latitude") || p.ends_with(".longitude") {
        "degrees"
    } else if p.ends_with(".orientation_rotation") {
        "steps"
    } else if p.ends_with(".night_start_minute") || p.ends_with(".night_end_minute") {
        "minute_of_day"
    } else if p.ends_with(".night_brightness_pct") {
        "percent"
    } else if p.ends_with(".freq") {
        "Hz"
    } else if p.ends_with("_db") {
        "dB"
    } else if p.ends_with("_dbfs") {
        "dBFS"
    } else if p.ends_with(".q") {
        "Q"
    } else if p.ends_with(".mix_gain") || p.ends_with(".volume_cap") {
        "ratio"
    } else if p.ends_with(".kws_threshold")
        || p.ends_with(".kws_score")
        || p.ends_with(".vad_threshold")
    {
        "score"
    } else if p.ends_with(".retries")
        || p.ends_with(".max_items")
        || p.ends_with(".max_sources")
        || p.ends_with(".count")
        || p.ends_with(".followup_credits")
        || p.ends_with("_sentences")
    {
        "count"
    } else if p.ends_with("_chars") {
        "chars"
    } else if p.ends_with("_url") || p.ends_with(".endpoint") || p.ends_with(".url") {
        "url"
    } else if p.ends_with(".api_key_env") {
        "env_var"
    } else if p == "persona.agent_md.content" {
        "markdown"
    } else if p.ends_with(".timezone") {
        "iana_timezone"
    } else if field.kind == "array" {
        "list"
    } else if field.kind == "object" {
        "map_or_struct"
    } else if field.kind == "json" {
        "json"
    } else {
        "string"
    }
    .into();

    let num_bounds = |min: Option<f64>, max: Option<f64>| Bounds {
        min,
        max,
        ..Default::default()
    };
    field.bounds = match p {
        "persona.agent_md.content" => Some(Bounds {
            max_len: Some(crate::agent_md::MAX_AGENT_MD_CHARS),
            ..Default::default()
        }),
        "music.library.endpoint"
        | "music.library.user"
        | "music.online.endpoint"
        | "music.online.user" => Some(Bounds {
            max_len: Some(crate::config::MAX_MUSIC_CONNECTION_FIELD),
            ..Default::default()
        }),
        "openapi.auth_header" => Some(Bounds {
            max_len: Some(64),
            ..Default::default()
        }),
        "open_sockets" => Some(Bounds {
            max_items: Some(crate::tool::open_socket::MAX_OPEN_SOCKETS),
            ..Bounds::default()
        }),
        "openapi.operations" => Some(Bounds {
            max_items: Some(crate::config::MAX_OPENAPI_OPERATIONS),
            ..Default::default()
        }),
        "openapi.operations[].params" => Some(Bounds {
            max_items: Some(crate::config::MAX_OPENAPI_PARAMS),
            ..Default::default()
        }),
        "openapi.operations[].params[].enum_values" => Some(Bounds {
            max_items: Some(32),
            ..Default::default()
        }),
        "mode.assistant_name" => Some(Bounds {
            max_len: Some(64),
            ..Default::default()
        }),
        "mode.persona_prompt" | "mode.system_prompt_override" => Some(Bounds {
            max_len: Some(4000),
            ..Default::default()
        }),
        "mode.follow_window_ms" => Some(num_bounds(Some(5_000.0), Some(60_000.0))),
        "mode.context_window_ms" => Some(num_bounds(Some(60_000.0), Some(600_000.0))),
        "mode.followup_credits" => Some(num_bounds(Some(0.0), Some(5.0))),
        "weather.max_days" => Some(num_bounds(Some(1.0), Some(7.0))),
        "weather.places[].latitude" => Some(num_bounds(Some(-90.0), Some(90.0))),
        "weather.places[].longitude" => Some(num_bounds(Some(-180.0), Some(180.0))),
        "home.timeout_ms" | "news.timeout_ms" | "search.timeout_ms" | "openapi.timeout_ms" => {
            Some(num_bounds(Some(500.0), Some(15_000.0)))
        }
        "open_sockets[].timeout_ms" => Some(num_bounds(Some(500.0), Some(30_000.0))),
        "home.retries" | "openapi.retries" => Some(num_bounds(Some(0.0), Some(3.0))),
        "news.max_sources" => Some(num_bounds(
            Some(1.0),
            Some(crate::config::MAX_NEWS_SOURCES_CAP as f64),
        )),
        "news.sources[].max_items" => Some(num_bounds(
            Some(1.0),
            Some(crate::config::MAX_NEWS_ITEMS as f64),
        )),
        "search.count" => Some(num_bounds(
            Some(1.0),
            Some(crate::config::MAX_SEARCH_COUNT as f64),
        )),
        "mode.retreat_words"
        | "mode.stop_intents.stop_words"
        | "mode.stop_intents.continue_words" => Some(Bounds {
            max_items: Some(12),
            ..Bounds::default()
        }),
        "mode.spoken.max_sentences" | "mode.spoken.detail_max_sentences" => {
            Some(num_bounds(Some(1.0), Some(8.0)))
        }
        "mode.spoken.max_chars" | "mode.spoken.detail_max_chars" => {
            Some(num_bounds(Some(40.0), Some(600.0)))
        }
        "mode.spoken.backstop_max_sentences" => Some(num_bounds(Some(2.0), Some(1000.0))),
        "mode.spoken.backstop_max_chars" => Some(num_bounds(Some(200.0), Some(6000.0))),
        "mode.filler.delay_ms"
        | "mode.filler.tool_delay_ms"
        | "mode.barge_protect_ms"
        | "mode.tail_blind_ms" => Some(num_bounds(Some(0.0), None)),
        "audio_eq.curve" => Some(Bounds {
            allowed: Some(vec![
                "rui".into(),
                "official".into(),
                "echomuse".into(),
                "custom".into(),
            ]),
            ..Default::default()
        }),
        "audio_eq.bands" => Some(Bounds {
            max_items: Some(10),
            ..Default::default()
        }),
        "audio_eq.bands[].freq" => Some(num_bounds(Some(20.0), Some(16_000.0))),
        "audio_eq.bands[].gain_db" => Some(num_bounds(Some(-18.0), Some(18.0))),
        "audio_eq.bands[].q" => Some(num_bounds(Some(0.3), Some(8.0))),
        "device_led_configs.*.night_start_minute" | "device_led_configs.*.night_end_minute" => {
            Some(num_bounds(Some(0.0), Some(1439.0)))
        }
        "device_led_configs.*.night_brightness_pct" => Some(num_bounds(Some(0.0), Some(100.0))),
        "device_led_configs.*.orientation_rotation" => Some(num_bounds(Some(0.0), Some(11.0))),
        "unit_acoustic_profiles.*.volume_cap" => Some(num_bounds(Some(0.0), Some(1.0))),
        "unit_acoustic_profiles.*.mix_gain" => Some(num_bounds(Some(0.05), Some(2.0))),
        "unit_acoustic_profiles.*.vad_threshold" => Some(num_bounds(Some(0.05), Some(0.99))),
        "unit_acoustic_profiles.*.onset_threshold_dbfs" => {
            Some(num_bounds(Some(-120.0), Some(0.0)))
        }
        "unit_acoustic_profiles.*.mic_gain_offset_db" => Some(num_bounds(Some(-24.0), Some(24.0))),
        "mode.duplex_requested.*" => Some(Bounds {
            allowed: Some(vec!["half".into(), "full".into()]),
            ..Default::default()
        }),
        "mode.kws_threshold" => Some(num_bounds(Some(0.01), Some(1.0))),
        "mode.kws_score" => Some(num_bounds(Some(0.1), Some(20.0))),
        _ if field.kind == "string" => Some(Bounds {
            max_len: Some(4096),
            ..Default::default()
        }),
        _ => None,
    };

    if !field.supported {
        let owner = if p.starts_with("music.") {
            Some("config::MusicConfig")
        } else if p.starts_with("weather.") {
            Some("config::WeatherConfig")
        } else if p.starts_with("home.") {
            Some("config::HomeConfig")
        } else if p.starts_with("news.") {
            Some("config::NewsConfig")
        } else if p.starts_with("search.") {
            Some("config::SearchConfig")
        } else if p.starts_with("openapi.") {
            Some("config::OpenApiConfig")
        } else if p.starts_with("mode.") && field.scope != "unit" {
            Some("voice::VoiceSettings")
        } else if p.starts_with("privacy.") {
            Some("voice::VoiceSettings::privacy")
        } else if p.starts_with("reflex.") {
            Some("config::ReflexConfig")
        } else if p.starts_with("devices.") {
            Some("config::DeviceMeta")
        } else if p.starts_with("audio_eq.") {
            Some("config::AudioEqConfig")
        } else if p.starts_with("device_led_configs.") || p == "node.led.timezone" {
            Some("config::DeviceLedConfig")
        } else if p == "node.wake.phrases" {
            Some("config::WakeConfig")
        } else if p.starts_with("open_sockets") {
            Some("tool::open_socket::OpenSocketContract")
        } else if p.starts_with("fleet.") {
            Some("fleet::FleetConfigDoc")
        } else if p == "db.maintenance" {
            Some("store::Store")
        } else {
            None
        };
        if let Some(o) = owner {
            field.default_source = o.into();
            field.validator = o.into();
        }
    }

    field.clear_behavior = if field.scope == "unit" {
        "protected_unit_calibration"
    } else if matches!(field.scope.as_str(), "boot" | "secret" | "data" | "runtime") {
        "protected_authority"
    } else if field.supported {
        "remove_layer_override"
    } else if field.nullable {
        "set_null_or_remove_override"
    } else if matches!(field.kind.as_str(), "array" | "object") {
        "replace_collection_or_remove_override"
    } else {
        "remove_layer_override"
    }
    .into();
}

/// A map wildcard denotes one opaque peer-id key (which may contain dots).
/// Array indices use canonical bracket notation. This lookup is metadata,
/// not write authority: validate_path still rejects every unsupported match.
fn path_matches(pattern: &str, path: &str) -> bool {
    fn matches(parts: &[&str], actual: &[&str]) -> bool {
        let Some((part, rest)) = parts.split_first() else {
            return actual.is_empty();
        };
        if *part == "*" {
            return (1..=actual.len()).any(|count| {
                crate::fleet::valid_peer_id(&actual[..count].join("."))
                    && matches(rest, &actual[count..])
            });
        }
        let Some((value, tail)) = actual.split_first() else {
            return false;
        };
        let equal = if let Some(prefix) = part.strip_suffix("[]") {
            value
                .strip_prefix(prefix)
                .and_then(|v| v.strip_prefix('['))
                .and_then(|v| v.strip_suffix(']'))
                .is_some_and(|index| !index.is_empty() && index.bytes().all(|c| c.is_ascii_digit()))
        } else {
            part == value
        };
        equal && matches(rest, tail)
    }
    pattern == path
        || matches(
            &pattern.split('.').collect::<Vec<_>>(),
            &path.split('.').collect::<Vec<_>>(),
        )
}

pub fn descriptor(path: &str) -> Option<FieldDescriptor> {
    if path.len() > 512 {
        return None;
    }
    fields()
        .into_iter()
        .find(|field| path_matches(&field.path, path))
}

pub fn validate_descriptor_value(desc: &FieldDescriptor, path: &str, v: &Value) -> Result<()> {
    if v.is_null() {
        return if desc.nullable {
            Ok(())
        } else {
            Err(
                DeliveryError::new("INVALID_FIELD_TYPE", format!("expected {}", desc.kind))
                    .at(path),
            )
        };
    }
    let kind_ok = match desc.kind.as_str() {
        "json" => true,
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.as_f64().is_some_and(f64::is_finite),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => false,
    };
    if !kind_ok {
        return Err(
            DeliveryError::new("INVALID_FIELD_TYPE", format!("expected {}", desc.kind)).at(path),
        );
    }
    if let Some(s) = v.as_str() {
        if s.contains('\0') {
            return Err(
                DeliveryError::new("INVALID_FIELD_VALUE", "field contains NUL byte").at(path),
            );
        }
        if let Some(max_len) = desc.bounds.as_ref().and_then(|b| b.max_len) {
            if s.chars().count() > max_len {
                return Err(DeliveryError::new(
                    "INVALID_FIELD_VALUE",
                    format!("string length exceeds {max_len}"),
                )
                .at(path));
            }
        }
        if let Some(allowed) = desc.bounds.as_ref().and_then(|b| b.allowed.as_ref()) {
            if !allowed.iter().any(|a| a == s) {
                return Err(DeliveryError::new(
                    "INVALID_FIELD_VALUE",
                    "value is not in allowed set",
                )
                .at(path));
            }
        }
    }
    if matches!(desc.kind.as_str(), "integer" | "number") {
        let num = v.as_f64().unwrap_or(f64::NAN);
        if !num.is_finite() {
            return Err(
                DeliveryError::new("INVALID_FIELD_VALUE", "numeric value must be finite").at(path),
            );
        }
        if let Some(bounds) = &desc.bounds {
            if let Some(min) = bounds.min {
                if num < min {
                    return Err(DeliveryError::new(
                        "INVALID_FIELD_VALUE",
                        format!("value {num} is below minimum {min}"),
                    )
                    .at(path));
                }
            }
            if let Some(max) = bounds.max {
                if num > max {
                    return Err(DeliveryError::new(
                        "INVALID_FIELD_VALUE",
                        format!("value {num} exceeds maximum {max}"),
                    )
                    .at(path));
                }
            }
        }
    }
    if let Some(arr) = v.as_array() {
        if let Some(max_items) = desc.bounds.as_ref().and_then(|b| b.max_items) {
            if arr.len() > max_items {
                return Err(DeliveryError::new(
                    "INVALID_FIELD_VALUE",
                    format!("array length exceeds {max_items}"),
                )
                .at(path));
            }
        }
        if let Some(item_kind) = desc.array_item_kind.as_deref() {
            for (idx, item) in arr.iter().enumerate() {
                let item_ok = match item_kind {
                    "json" => true,
                    "string" => item.is_string(),
                    "boolean" => item.is_boolean(),
                    "integer" => item.is_i64() || item.is_u64(),
                    "number" => item.as_f64().is_some_and(f64::is_finite),
                    "array" => item.is_array(),
                    "object" => item.is_object(),
                    _ => false,
                };
                if !item_ok {
                    return Err(DeliveryError::new(
                        "INVALID_FIELD_TYPE",
                        format!("expected array item of kind {item_kind}"),
                    )
                    .at(&format!("{path}[{idx}]")));
                }
            }
        }
    }
    Ok(())
}

pub fn validate_path(
    path: &str,
    purpose: &Purpose,
    value: Option<&Value>,
) -> Result<FieldDescriptor> {
    let desc = descriptor(path).ok_or_else(|| {
        DeliveryError::new("UNKNOWN_FIELD", "unregistered configuration path").at(path)
    })?;
    if !desc.supported {
        return Err(DeliveryError::new(
            "UNSUPPORTED_FIELD",
            "registered but protected/unsupported in this slice",
        )
        .at(path));
    }
    if *purpose == Purpose::FamilyPolicy && desc.scope != "family_or_node" {
        return Err(DeliveryError::new(
            "FORBIDDEN_SCOPE",
            "family policy cannot write node/unit fields",
        )
        .at(path));
    }
    if *purpose == Purpose::FamilyPolicy && path == "reflex.words" {
        if let Some(items) = value.and_then(Value::as_array) {
            for (idx, item) in items.iter().enumerate() {
                if item.get("intent").and_then(Value::as_str) == Some("wake") {
                    return Err(DeliveryError::new(
                        "FORBIDDEN_SCOPE",
                        "family policy cannot write reflex words with intent 'wake'",
                    )
                    .at(&format!("reflex.words[{idx}]")));
                }
            }
        }
    }
    if let Some(v) = value {
        validate_descriptor_value(&desc, path, v)?;
    }
    Ok(desc)
}

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod tests;
