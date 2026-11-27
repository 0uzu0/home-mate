use super::*;
use crate::{config::*, voice::*};
use serde::{
    de::{DeserializeOwned, Error as _, Visitor},
    Serialize,
};
use serde_json::json;

// Serde's generated visitor supplies the actual Rust field list, including
// skip-serializing Option fields. Adding a field to a covered struct therefore
// fails coverage without maintaining a duplicate handwritten expected list.
struct Capture<'a>(&'a mut Vec<&'static str>);
impl<'de> serde::Deserializer<'de> for Capture<'_> {
    type Error = serde::de::value::Error;
    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> std::result::Result<V::Value, Self::Error> {
        Err(Self::Error::custom("field discovery only"))
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        self.0.extend_from_slice(fields);
        Err(Self::Error::custom("field discovery complete"))
    }
    serde::forward_to_deserialize_any! {bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any}
}
fn accepts(kind: &str, value: &Value) -> bool {
    match kind {
        "json" => true,
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}
fn observe(path: &str, value: &Value) {
    let desc = descriptor(path).unwrap_or_else(|| panic!("missing serialized field {path}"));
    if value.is_null() {
        assert!(
            desc.nullable,
            "{path} is nullable in its actual serde representation"
        );
        return;
    }
    assert!(
        accepts(&desc.kind, value),
        "{path}: descriptor {} differs from {value}",
        desc.kind
    );
    if desc.kind == "json" {
        return;
    }
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                observe(&format!("{path}.{key}"), value);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                assert!(
                    accepts(
                        desc.array_item_kind
                            .as_deref()
                            .expect("array element metadata"),
                        value
                    ),
                    "{path} element kind"
                );
                if let Value::Object(fields) = value {
                    for (key, value) in fields {
                        observe(&format!("{path}[{index}].{key}"), value);
                    }
                }
            }
        }
        _ => {}
    }
}
fn check<T: DeserializeOwned + Serialize>(prefix: &str, fixture: Value) {
    let mut names = vec![];
    let _ = T::deserialize(Capture(&mut names));
    assert!(
        !names.is_empty(),
        "{prefix}: serde struct field discovery must execute"
    );
    for name in names {
        let path = format!("{prefix}.{name}");
        let desc = descriptor(&path).unwrap_or_else(|| panic!("missing actual serde field {path}"));
        let mut with_null = fixture.clone();
        with_null[name] = Value::Null;
        assert_eq!(
            serde_json::from_value::<T>(with_null).is_ok(),
            desc.nullable,
            "{path}: nullability must match actual deserialization"
        );
    }
    let native: T = serde_json::from_value(fixture).unwrap();
    for (name, value) in serde_json::to_value(native).unwrap().as_object().unwrap() {
        observe(&format!("{prefix}.{name}"), value);
    }
}
fn defaults<T: DeserializeOwned + Serialize + Default>(prefix: &str) {
    check::<T>(prefix, serde_json::to_value(T::default()).unwrap());
}

#[test]
fn current_native_serde_configuration_fields_and_values_are_registered() {
    defaults::<ServicesConfig>("services");
    for slot in ["llm", "llm_fallback", "asr", "tts"] {
        defaults::<ServiceConfig>(&format!("services.{slot}"));
    }
    defaults::<SharedMemoryConfig>("shared_memory");
    defaults::<MusicConfig>("music");
    for slot in ["library", "online"] {
        defaults::<MusicConnection>(&format!("music.{slot}"));
    }
    defaults::<WeatherConfig>("weather");
    check::<WeatherPlace>(
        "weather.places[0]",
        json!({"alias":"fixture","name":"fixture","latitude":31.0,"longitude":121.0,"timezone":"Asia/Shanghai","city_code":""}),
    );
    defaults::<HomeConfig>("home");
    check::<HomeEntity>(
        "home.entities[0]",
        json!({"name":"fixture","entity":"light.fixture"}),
    );
    defaults::<NewsConfig>("news");
    check::<NewsSource>(
        "news.sources[0]",
        json!({"alias":"fixture","url":"http://127.0.0.1/synthetic"}),
    );
    defaults::<SearchConfig>("search");
    defaults::<OpenApiConfig>("openapi");
    check::<OpenApiOperation>(
        "openapi.operations[0]",
        json!({"id":"fixture","path":"/fixture"}),
    );
    check::<OpenApiParam>("openapi.operations[0].params[0]", json!({"name":"fixture"}));
    defaults::<VoiceSettings>("mode");
    defaults::<WakeAckSettings>("mode.wake_ack");
    defaults::<StopIntentSettings>("mode.stop_intents");
    defaults::<FillerSettings>("mode.filler");
    defaults::<SpokenDeliverySettings>("mode.spoken");
    check::<VadSettings>("mode.vad", json!({"start_db":-40.0,"end_db":-50.0}));
    defaults::<ReflexConfig>("reflex");
    check::<ReflexWord>(
        "reflex.words[0]",
        json!({"text":"fixture","tokens":"f i","intent":"wake","enabled":true}),
    );
    defaults::<DeviceMeta>("devices.dot2-d");
    defaults::<AudioEqConfig>("audio_eq");
    check::<EqBandCfg>(
        "audio_eq.bands[0]",
        json!({"freq":100.0,"gain_db":0.0,"q":1.0}),
    );
    defaults::<UnitAcousticProfile>("unit_acoustic_profiles.dot2-d");
    defaults::<DeviceLedConfig>("device_led_configs.dot2-d");
    check::<crate::tool::open_socket::OpenSocketContract>(
        "open_sockets[0]",
        json!({"name":"fixture","description":"fixture","effect":"Query","endpoint_url":"http://127.0.0.1/synthetic"}),
    );
    defaults::<crate::fleet::FleetConfigDoc>("fleet");
    check::<crate::fleet::PeerNode>(
        "fleet.nodes[0]",
        json!({"id":"fixture","base_url":"http://127.0.0.1:1","added_ms":0}),
    );
    for slot in [
        "story",
        "joke",
        "radio",
        "learning",
        "services.story",
        "services.joke",
        "services.radio",
        "services.learning",
    ] {
        defaults::<PersonalEndpointConfig>(slot);
    }
}

#[test]
fn optional_scalars_containers_and_opaque_json_report_their_actual_shapes() {
    let voice:VoiceSettings=serde_json::from_value(json!({"system_prompt_override":null,"followup_credits":null,"vad":null,"wake_ack":{"resynth":null},"filler":{"resynth":null}})).unwrap();
    assert!(
        voice.system_prompt_override.is_none()
            && voice.followup_credits.is_none()
            && voice.vad.is_none()
    );
    for path in [
        "music.library",
        "music.online",
        "mode.system_prompt_override",
        "mode.followup_credits",
        "mode.vad",
        "mode.wake_ack.resynth",
        "mode.filler.resynth",
        "openapi.operations[].verification",
        "unit_acoustic_profiles.*.onset_threshold_dbfs",
    ] {
        assert!(descriptor(path).unwrap().nullable, "{path}");
    }
    check::<VoiceSettings>(
        "mode",
        json!({"system_prompt_override":"fixture","followup_credits":2,"vad":{"start_db":-40.0,"end_db":-50.0},"duplex_requested":{"dot2-d":"half"},"wake_ack":{"resynth":true},"filler":{"resynth":false}}),
    );
    check::<MusicConfig>(
        "music",
        json!({"library":{"endpoint":"http://127.0.0.1","user":"fixture"},"online":null}),
    );
    for expected in [
        Value::Null,
        json!(true),
        json!(3),
        json!("fixture"),
        json!([1]),
        json!({"fixture":1}),
    ] {
        let variants = [
            VerificationStrategy::ReadbackOp {
                op_id: "fixture".into(),
                json_path: "/fixture".into(),
                expected: expected.clone(),
            },
            VerificationStrategy::ResponseAssert {
                json_path: "/fixture".into(),
                expected,
            },
            VerificationStrategy::TransportOnly,
        ];
        for variant in variants {
            // Exhaustive matching also makes a newly added enum variant a
            // required coverage decision instead of a silently skipped branch.
            match &variant {
                VerificationStrategy::ReadbackOp { .. }
                | VerificationStrategy::ResponseAssert { .. }
                | VerificationStrategy::TransportOnly => {}
            }
            observe(
                "openapi.operations[0].verification",
                &serde_json::to_value(variant).unwrap(),
            );
        }
    }
    assert_eq!(
        descriptor("unit_acoustic_profiles.dot2-d.mic_calibrated")
            .unwrap()
            .kind,
        "boolean"
    );
    assert_eq!(
        descriptor("unit_acoustic_profiles.dot2-d.volume_cap")
            .unwrap()
            .kind,
        "number"
    );
}

#[test]
fn concrete_wildcards_stay_protected_and_catalog_never_grants_new_apply_authority() {
    for path in [
        "devices.dot2-d.name",
        "devices.zone.a.room",
        "weather.places[0].latitude",
        "open_sockets[0].parameters",
        "unit_acoustic_profiles.dot2-d.mic_calibrated",
    ] {
        assert_eq!(
            validate_path(path, &Purpose::NodeConfig, Some(&json!("synthetic")))
                .unwrap_err()
                .code,
            "UNSUPPORTED_FIELD",
            "{path}"
        );
    }
    for path in [
        "weather.places[bad].latitude",
        "open_sockets[-1].name",
        "device_led_configs..night_enabled",
        "services.llm.missing",
        "devices.dot2-d.missing",
    ] {
        assert!(descriptor(path).is_none(), "{path}");
    }
    let catalog = fields();
    let names = catalog
        .iter()
        .map(|field| field.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), catalog.len(), "duplicate descriptors");
    assert_eq!(
        catalog.iter().filter(|field| field.supported).count(),
        127,
        "descriptive coverage must not enable unexpected fields"
    );
    for path in [
        "mode.wake_ack.enabled",
        "mode.wake_ack.phrases",
        "mode.retreat_ack",
        "mode.window_close_ack",
        "mode.truncation_notice",
        "mode.filler.mode",
        "mode.filler.delay_ms",
        "mode.filler.tool_delay_ms",
        "mode.filler.thinking",
        "mode.filler.search",
        "mode.filler.device",
        "mode.filler.story",
    ] {
        assert!(
            descriptor(path).unwrap().supported,
            "{path} should be supported"
        );
    }
    for path in [
        "mode.wake_words",
        "mode.kws_threshold",
        "mode.kws_score",
        "mode.duplex_requested.dot2-d",
        "node.wake.phrases",
        "node.led.timezone",
        "reflex.words",
        "audio_eq.curve",
        "audio_eq.bands",
        "device_led_configs.dot2-d.orientation_rotation",
        "unit_acoustic_profiles.dot2-d.volume_cap",
        "unit_acoustic_profiles.dot2-d.mix_gain",
        "unit_acoustic_profiles.dot2-d.onset_threshold_dbfs",
    ] {
        assert!(
            descriptor(path).unwrap().supported,
            "{path} should be supported"
        );
    }
    for path in [
        "mode.vad.start_db",
        "mode.vad.end_db",
        "mode.vad_by_device",
        "mode.vad_by_device.dot2-d.start_db",
        "mode.vad_by_device.dot2-d.end_db",
        "mode.media_trim_db",
        "mode.media_trim_db_by_device",
        "mode.media_trim_db_by_device.dot2-d",
        "mode.device_overrides",
        "mode.device_overrides.dot2-d.tail_blind_ms",
        "mode.tail_blind_ms",
        "unit_acoustic_profiles.dot2-d.mic_calibrated",
        "mode.wake_ack.resynth",
        "mode.filler.resynth",
    ] {
        assert!(
            !descriptor(path).unwrap().supported,
            "{path} must remain gated"
        );
    }
    for field in catalog.iter().filter(|field| !field.supported) {
        assert!(field.purposes.is_empty());
        assert_eq!(field.apply, "unsupported_in_slice");
    }
    assert_eq!(
        validate_path(
            "services.llm.model",
            &Purpose::NodeConfig,
            Some(&Value::Null)
        )
        .unwrap_err()
        .code,
        "INVALID_FIELD_TYPE"
    );
}

#[test]
fn all_descriptors_have_typed_metadata_bounds_and_unit_isolation() {
    let catalog = fields();
    assert_eq!(catalog.len(), 268);
    for field in &catalog {
        assert!(!field.unit.is_empty(), "{} missing unit", field.path);
        assert!(
            !field.clear_behavior.is_empty(),
            "{} missing clear_behavior",
            field.path
        );
        assert!(
            !field.default_source.is_empty(),
            "{} missing default_source",
            field.path
        );
        assert!(
            !field.validator.is_empty(),
            "{} missing validator",
            field.path
        );
        if field.scope == "unit" {
            assert_eq!(field.clear_behavior, "protected_unit_calibration");
            if field.supported {
                assert_eq!(field.purposes, vec!["node_config", "same_node_restore"]);
            }
        }
    }

    let weather_days = descriptor("weather.max_days").unwrap();
    assert_eq!(weather_days.kind, "integer");
    assert_eq!(weather_days.unit, "days");
    assert!(validate_descriptor_value(&weather_days, "weather.max_days", &json!(3)).is_ok());
    assert_eq!(
        validate_descriptor_value(&weather_days, "weather.max_days", &json!(0))
            .unwrap_err()
            .code,
        "INVALID_FIELD_VALUE"
    );
    assert_eq!(
        validate_descriptor_value(&weather_days, "weather.max_days", &json!(3.5))
            .unwrap_err()
            .code,
        "INVALID_FIELD_TYPE"
    );

    let lat = descriptor("weather.places[0].latitude").unwrap();
    assert_eq!(lat.kind, "number");
    assert_eq!(lat.unit, "degrees");
    assert!(validate_descriptor_value(&lat, "weather.places[0].latitude", &json!(31.23)).is_ok());
    assert_eq!(
        validate_descriptor_value(&lat, "weather.places[0].latitude", &json!(95.0))
            .unwrap_err()
            .code,
        "INVALID_FIELD_VALUE"
    );

    let retreat = descriptor("mode.retreat_words").unwrap();
    assert_eq!(retreat.kind, "array");
    assert_eq!(retreat.array_item_kind.as_deref(), Some("string"));

    let max_sources = descriptor("news.max_sources").unwrap();
    assert_eq!(max_sources.kind, "integer");
    assert_eq!(max_sources.unit, "count");
    assert!(validate_path("news.max_sources", &Purpose::FamilyPolicy, Some(&json!(12))).is_ok());
    assert_eq!(
        validate_path("news.max_sources", &Purpose::FamilyPolicy, Some(&json!(0)))
            .unwrap_err()
            .code,
        "INVALID_FIELD_VALUE"
    );
    assert_eq!(
        validate_path("news.max_sources", &Purpose::FamilyPolicy, Some(&json!(51)))
            .unwrap_err()
            .code,
        "INVALID_FIELD_VALUE"
    );
    assert!(validate_descriptor_value(&retreat, "mode.retreat_words", &json!(["退下"])).is_ok());
    assert_eq!(
        validate_descriptor_value(&retreat, "mode.retreat_words", &json!([123]))
            .unwrap_err()
            .code,
        "INVALID_FIELD_TYPE"
    );

    let sys_prompt = descriptor("mode.system_prompt_override").unwrap();
    assert!(sys_prompt.nullable);
    assert!(
        validate_descriptor_value(&sys_prompt, "mode.system_prompt_override", &Value::Null).is_ok()
    );

    let expected = descriptor("openapi.operations[0].verification.expected").unwrap();
    assert_eq!(expected.kind, "json");
    assert!(expected.nullable);
    assert!(validate_descriptor_value(
        &expected,
        "openapi.operations[0].verification.expected",
        &json!({"ok":true})
    )
    .is_ok());
    assert!(validate_descriptor_value(
        &expected,
        "openapi.operations[0].verification.expected",
        &Value::Null
    )
    .is_ok());

    assert_eq!(
        validate_path(
            "services.llm.api_key_env",
            &Purpose::FamilyPolicy,
            Some(&json!("DEEPSEEK_API_KEY"))
        )
        .unwrap_err()
        .code,
        "FORBIDDEN_SCOPE"
    );
    assert_eq!(
        validate_path(
            "unit_acoustic_profiles.dot2-d.volume_cap",
            &Purpose::FamilyPolicy,
            Some(&json!(0.8))
        )
        .unwrap_err()
        .code,
        "FORBIDDEN_SCOPE"
    );
    assert_eq!(
        validate_path(
            "mode.kws_threshold",
            &Purpose::FamilyPolicy,
            Some(&json!(0.15))
        )
        .unwrap_err()
        .code,
        "FORBIDDEN_SCOPE"
    );
    let vol_cap = descriptor("unit_acoustic_profiles.dot2-d.volume_cap").unwrap();
    assert_eq!(vol_cap.unit, "ratio");
    assert!(validate_descriptor_value(
        &vol_cap,
        "unit_acoustic_profiles.dot2-d.volume_cap",
        &json!(0.8)
    )
    .is_ok());
    assert!(validate_descriptor_value(
        &vol_cap,
        "unit_acoustic_profiles.dot2-d.volume_cap",
        &json!(50.0)
    )
    .is_err());

    let mix = descriptor("unit_acoustic_profiles.dot2-d.mix_gain").unwrap();
    assert!(
        validate_descriptor_value(&mix, "unit_acoustic_profiles.dot2-d.mix_gain", &json!(1.2))
            .is_ok()
    );
    assert!(
        validate_descriptor_value(&mix, "unit_acoustic_profiles.dot2-d.mix_gain", &json!(2.5))
            .is_err()
    );

    let onset = descriptor("unit_acoustic_profiles.dot2-d.onset_threshold_dbfs").unwrap();
    assert_eq!(onset.unit, "dBFS");
    assert!(onset.nullable);
    assert!(validate_descriptor_value(
        &onset,
        "unit_acoustic_profiles.dot2-d.onset_threshold_dbfs",
        &json!(-38.0)
    )
    .is_ok());
    assert!(validate_descriptor_value(
        &onset,
        "unit_acoustic_profiles.dot2-d.onset_threshold_dbfs",
        &Value::Null
    )
    .is_ok());
    assert!(validate_descriptor_value(
        &onset,
        "unit_acoustic_profiles.dot2-d.onset_threshold_dbfs",
        &json!(5.0)
    )
    .is_err());
    assert_eq!(
        validate_path(
            "unit_acoustic_profiles.dot2-d.onset_threshold_dbfs",
            &Purpose::FamilyPolicy,
            Some(&json!(-38.0))
        )
        .unwrap_err()
        .code,
        "FORBIDDEN_SCOPE"
    );

    let rot = descriptor("device_led_configs.dot2-d.orientation_rotation").unwrap();
    assert_eq!(rot.unit, "steps");
    assert!(validate_descriptor_value(
        &rot,
        "device_led_configs.dot2-d.orientation_rotation",
        &json!(6)
    )
    .is_ok());
    assert!(validate_descriptor_value(
        &rot,
        "device_led_configs.dot2-d.orientation_rotation",
        &json!(90)
    )
    .is_err());

    let eq_freq = descriptor("audio_eq.bands[0].freq").unwrap();
    assert!(validate_descriptor_value(&eq_freq, "audio_eq.bands[0].freq", &json!(16000.0)).is_ok());
    assert!(
        validate_descriptor_value(&eq_freq, "audio_eq.bands[0].freq", &json!(18000.0)).is_err()
    );
}
