//! Gateway v2 (`config_apply_v2`) preflight, dispatch, receipt awaiting,
//! and rollback compensation for `ConfigApplyCoordinator`.

use super::{digest, DeliveryError, Result};
use crate::config::{
    compile_keywords, reflex_intent_map, reflex_tokens, AudioEqConfig, DeviceLedConfig,
    ReflexConfig, ReflexIntent, ReflexWord, UnitAcousticProfile,
};
use crate::gateway::{
    ConfigApplyRequest, ConfigBlock, ConfigReceiptStatus, ConfigValidateRequest, DeviceHub,
    GatewayFrame,
};
use crate::id;
use serde_json::{json, Value};
use std::time::Duration;

const DEFAULT_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_APPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct GatewayDispatchOutcome {
    pub(crate) state: &'static str,
    pub(crate) error: Option<String>,
    pub(crate) receipt: Value,
}

fn merge_wake_phrases_into_reflex(
    base: Option<&ReflexConfig>,
    phrases: &[String],
    err_path: &str,
) -> Result<ReflexConfig> {
    let mut rfx = base.cloned().unwrap_or_default();
    let cleaned: Vec<String> = phrases
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        return Ok(rfx);
    }
    let mut next_words = Vec::with_capacity(cleaned.len() + rfx.words.len());
    for phrase in &cleaned {
        if let Some(existing) = rfx.words.iter().find(|w| w.text.trim() == phrase) {
            let tokens = if existing.tokens.trim().is_empty() {
                reflex_tokens(phrase)
                    .map_err(|e| DeliveryError::new("INVALID_REFLEX", e).at(err_path))?
            } else {
                existing.tokens.clone()
            };
            next_words.push(ReflexWord {
                text: phrase.clone(),
                tokens,
                intent: ReflexIntent::Wake,
                enabled: true,
            });
        } else {
            let tokens = reflex_tokens(phrase)
                .map_err(|e| DeliveryError::new("INVALID_REFLEX", e).at(err_path))?;
            next_words.push(ReflexWord {
                text: phrase.clone(),
                tokens,
                intent: ReflexIntent::Wake,
                enabled: true,
            });
        }
    }
    for word in rfx.words {
        if word.intent != ReflexIntent::Wake && !cleaned.iter().any(|p| p == word.text.trim()) {
            next_words.push(word);
        }
    }
    rfx.words = next_words;
    Ok(rfx)
}

fn compile_kws_block(
    rfx: &ReflexConfig,
    threshold: Option<f64>,
    score: Option<f64>,
    err_path: &str,
) -> Result<Value> {
    let keywords =
        compile_keywords(rfx).map_err(|e| DeliveryError::new("INVALID_REFLEX", e).at(err_path))?;
    let (retreat_words, intents_hash) = reflex_intent_map(rfx);
    let mut kws = json!({
        "keywords": keywords,
        "retreat_words": retreat_words,
        "intents_hash": intents_hash,
    });
    if let Some(t) = threshold {
        kws["threshold"] = json!(t);
    }
    if let Some(s) = score {
        kws["score"] = json!(s);
    }
    Ok(kws)
}

fn build_led_payload(led: &DeviceLedConfig, timezone: &str) -> Value {
    let tz = if timezone.trim().is_empty() {
        "Asia/Shanghai"
    } else {
        timezone.trim()
    };
    json!({
        "night_enabled": led.night_enabled,
        "night_start_minute": led.night_start_minute,
        "night_end_minute": led.night_end_minute,
        "night_brightness_pct": led.night_brightness_pct,
        "orientation_rotation": led.orientation_rotation,
        "orientation_reverse": led.orientation_reverse,
        "timezone": tz,
    })
}

/// Build the Gateway `ConfigBlock` and its contract-exact `values` JSON for
/// a given `ConfigDelivery` block.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_gateway_payload(
    block_name: &str,
    device: &str,
    desired: &Value,
    expected: Option<&Value>,
    current_voice: Option<&crate::voice::VoiceSettings>,
    current_reflex: Option<&ReflexConfig>,
    current_node: Option<&Value>,
    current_led: Option<&DeviceLedConfig>,
) -> Result<Vec<(ConfigBlock, Value)>> {
    let mut out = Vec::new();
    match block_name {
        "audio_eq" => {
            let eq: AudioEqConfig = serde_json::from_value(desired.clone())?;
            eq.validate()
                .map_err(|e| DeliveryError::new("INVALID_AUDIO_EQ", e).at("audio_eq"))?;
            let bands: Vec<Value> = eq
                .bands
                .iter()
                .map(|b| json!({ "freq": b.freq, "gain_db": b.gain_db, "q": b.q }))
                .collect();
            out.push((
                ConfigBlock::AudioEq,
                json!({ "curve": eq.curve, "bands": bands }),
            ));
        }
        "device_led_configs" => {
            let entry = desired.get(device).cloned().unwrap_or_else(|| json!({}));
            let led: DeviceLedConfig = serde_json::from_value(entry)?;
            led.validate().map_err(|e| {
                DeliveryError::new("INVALID_LED_CONFIG", e.to_string()).at("device_led_configs")
            })?;
            let tz = current_node
                .and_then(|n| n.get("led"))
                .and_then(|l| l.get("timezone"))
                .and_then(Value::as_str)
                .unwrap_or("Asia/Shanghai");
            out.push((ConfigBlock::Led, build_led_payload(&led, tz)));
        }
        "unit_acoustic_profiles" => {
            let entry = desired.get(device).cloned().unwrap_or_else(|| json!({}));
            let prof: UnitAcousticProfile = serde_json::from_value(entry)?;
            prof.validate().map_err(|e| {
                DeliveryError::new("INVALID_ACOUSTIC_PROFILE", e.to_string())
                    .at("unit_acoustic_profiles")
            })?;
            if prof.mix_gain > 2.0 {
                return Err(DeliveryError::new(
                    "INVALID_ACOUSTIC_PROFILE",
                    format!("mix_gain {} exceeds Gateway max 2.0", prof.mix_gain),
                ));
            }
            let onset_threshold_dbfs = prof
                .onset_threshold_dbfs
                .unwrap_or_else(|| {
                    current_voice
                        .and_then(|v| v.vad.as_ref().map(|vad| vad.start_db))
                        .unwrap_or(-40.0)
                })
                .clamp(-120.0, 0.0);
            out.push((
                ConfigBlock::Acoustics,
                json!({
                    "volume_cap": prof.volume_cap,
                    "mix_gain": prof.mix_gain,
                    "onset_threshold_dbfs": onset_threshold_dbfs,
                    "mic_gain_offset_db": prof.mic_gain_offset_db,
                }),
            ));
        }
        "reflex" => {
            let rfx: ReflexConfig = serde_json::from_value(desired.clone())?;
            let kws = compile_kws_block(
                &rfx,
                current_voice.map(|v| f64::from(v.kws_threshold)),
                current_voice.map(|v| f64::from(v.kws_score)),
                "reflex",
            )?;
            out.push((ConfigBlock::Kws, kws));
        }
        "node" => {
            let prev_led = expected.and_then(|e| e.get("led"));
            let prev_wake = expected.and_then(|e| e.get("wake"));
            let wake_changed = desired.get("wake") != prev_wake;
            let led_changed = desired.get("led") != prev_led;
            if led_changed || !wake_changed {
                if let Some(tz) = desired
                    .get("led")
                    .and_then(|l| l.get("timezone"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    let led = current_led.cloned().unwrap_or_default();
                    out.push((ConfigBlock::Led, build_led_payload(&led, tz)));
                }
            }
            if wake_changed {
                if let Some(phrases_arr) = desired
                    .get("wake")
                    .and_then(|w| w.get("phrases"))
                    .and_then(Value::as_array)
                {
                    let phrases: Vec<String> = phrases_arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    let rfx = merge_wake_phrases_into_reflex(
                        current_reflex,
                        &phrases,
                        "node.wake.phrases",
                    )?;
                    let kws = compile_kws_block(
                        &rfx,
                        current_voice.map(|v| f64::from(v.kws_threshold)),
                        current_voice.map(|v| f64::from(v.kws_score)),
                        "node.wake.phrases",
                    )?;
                    out.push((ConfigBlock::Kws, kws));
                }
            }
        }
        "mode" => {
            let prev_duplex = expected.and_then(|e| e.get("duplex_requested"));
            let next_duplex = desired.get("duplex_requested");
            if next_duplex != prev_duplex {
                if let Some(req) = next_duplex
                    .and_then(|m| m.get(device))
                    .and_then(Value::as_str)
                {
                    out.push((ConfigBlock::Duplex, json!({ "duplex_requested": req })));
                }
            }
            let prev_kws_t = expected.and_then(|e| e.get("kws_threshold"));
            let prev_kws_s = expected.and_then(|e| e.get("kws_score"));
            let prev_wake = expected.and_then(|e| e.get("wake_words"));
            if desired.get("kws_threshold") != prev_kws_t
                || desired.get("kws_score") != prev_kws_s
                || desired.get("wake_words") != prev_wake
            {
                let mut wake_words: Vec<String> = if desired.get("wake_words") != prev_wake {
                    desired
                        .get("wake_words")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                if wake_words.is_empty() {
                    if let Some(node_phrases) = current_node
                        .and_then(|n| n.get("wake"))
                        .and_then(|w| w.get("phrases"))
                        .and_then(Value::as_array)
                    {
                        wake_words = node_phrases
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                    }
                }
                let rfx =
                    merge_wake_phrases_into_reflex(current_reflex, &wake_words, "mode.wake_words")?;
                let threshold = desired
                    .get("kws_threshold")
                    .and_then(Value::as_f64)
                    .or_else(|| current_voice.map(|v| f64::from(v.kws_threshold)));
                let score = desired
                    .get("kws_score")
                    .and_then(Value::as_f64)
                    .or_else(|| current_voice.map(|v| f64::from(v.kws_score)));
                let kws = compile_kws_block(&rfx, threshold, score, "mode")?;
                out.push((ConfigBlock::Kws, kws));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Dispatch one Gateway `ConfigBlock` with preflight (`ConfigStateGet` +
/// `ConfigValidate`), `ConfigApply`, receipt awaiting (`Persisted -> Active`),
/// and one automatic retry on `retryable == true`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_block_v2(
    hub: &DeviceHub,
    device: &str,
    data_epoch: u64,
    application_id: &str,
    desired_revision: u64,
    block_generation: u64,
    block: ConfigBlock,
    values: Value,
) -> GatewayDispatchOutcome {
    dispatch_block_v2_inner(
        hub,
        device,
        data_epoch,
        application_id,
        desired_revision,
        block_generation,
        block,
        values,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_block_v2_clamped(
    hub: &DeviceHub,
    device: &str,
    data_epoch: u64,
    application_id: &str,
    desired_revision: u64,
    block_generation: u64,
    block: ConfigBlock,
    values: Value,
) -> GatewayDispatchOutcome {
    dispatch_block_v2_inner(
        hub,
        device,
        data_epoch,
        application_id,
        desired_revision,
        block_generation,
        block,
        values,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_block_v2_inner(
    hub: &DeviceHub,
    device: &str,
    data_epoch: u64,
    application_id: &str,
    desired_revision: u64,
    block_generation: u64,
    block: ConfigBlock,
    values: Value,
    clamp_generation: bool,
) -> GatewayDispatchOutcome {
    let block_hash = match digest(&values) {
        Ok(h) => h,
        Err(e) => {
            return GatewayDispatchOutcome {
                state: "failed",
                error: Some(format!("BLOCK_HASH_FAILED: {}", e.detail)),
                receipt: json!({ "block": block, "state": "failed", "reason": e.detail }),
            };
        }
    };

    for attempt in 0..2 {
        // 1. Preflight: ConfigStateGet to acquire boot_id + state_revision fencing.
        let (boot_id, state_rev, mut cap_fp, effective_gen) = match hub
            .config_state_get(device, DEFAULT_PREFLIGHT_TIMEOUT)
            .await
        {
            Ok(GatewayFrame::ConfigState {
                gateway_boot_id,
                state_revision,
                capability_fingerprint,
                blocks,
                ..
            }) => {
                let effective_gen = if clamp_generation {
                    let existing = blocks
                        .iter()
                        .filter(|b| b.block == block && b.data_epoch == data_epoch)
                        .map(|b| b.block_generation)
                        .max()
                        .unwrap_or(0);
                    block_generation.max(existing)
                } else {
                    block_generation
                };
                (
                    gateway_boot_id,
                    state_revision,
                    capability_fingerprint,
                    effective_gen,
                )
            }
            Ok(other) => {
                return GatewayDispatchOutcome {
                    state: "failed",
                    error: Some("GATEWAY_STATE_GET_UNEXPECTED".into()),
                    receipt: json!({ "block": block, "state": "failed", "detail": format!("{other:?}") }),
                };
            }
            Err(e) => {
                return GatewayDispatchOutcome {
                    state: "failed",
                    error: Some(format!("GATEWAY_PREFLIGHT_FAILED: {e}")),
                    receipt: json!({ "block": block, "state": "failed", "reason": e }),
                };
            }
        };

        // 2. Preflight: ConfigValidate
        match hub
            .config_validate(
                ConfigValidateRequest {
                    device: device.to_string(),
                    expected_gateway_boot_id: boot_id.clone(),
                    expected_state_revision: state_rev,
                    block,
                    schema_version: 1,
                    block_hash: block_hash.clone(),
                    values: values.clone(),
                },
                DEFAULT_PREFLIGHT_TIMEOUT,
            )
            .await
        {
            Ok(GatewayFrame::ConfigValidated {
                ok,
                supported,
                blockers,
                state,
                reason,
                retryable,
                capability_fingerprint,
                evidence,
                ..
            }) => {
                if capability_fingerprint.is_some() {
                    cap_fp = capability_fingerprint;
                }
                // The gateway answers `supported` + `blockers`; Core-shaped
                // peers answer `ok` + `state`. Accept either only when there
                // are zero blockers, and surface gateway blockers on rejection.
                let reason = reason.or_else(|| {
                    (!blockers.is_empty())
                        .then(|| format!("GATEWAY_BLOCKED: {}", blockers.join("; ")))
                });
                if !(ok || supported)
                    || !blockers.is_empty()
                    || matches!(
                        state,
                        ConfigReceiptStatus::Failed
                            | ConfigReceiptStatus::Unsupported
                            | ConfigReceiptStatus::Superseded
                    )
                {
                    if retryable && attempt == 0 {
                        continue;
                    }
                    let final_state = if state == ConfigReceiptStatus::Superseded {
                        "superseded"
                    } else {
                        "failed"
                    };
                    return GatewayDispatchOutcome {
                        state: final_state,
                        error: Some(
                            reason
                                .clone()
                                .unwrap_or_else(|| "GATEWAY_VALIDATE_REJECTED".into()),
                        ),
                        receipt: json!({
                            "block": block,
                            "state": final_state,
                            "supported": supported,
                            "blockers": blockers,
                            "reason": reason,
                            "retryable": retryable,
                            "evidence": evidence,
                            "gateway_boot_id": boot_id,
                            "state_revision": state_rev,
                        }),
                    };
                }
            }
            Ok(_) => {
                return GatewayDispatchOutcome {
                    state: "failed",
                    error: Some("GATEWAY_VALIDATE_UNEXPECTED".into()),
                    receipt: json!({ "block": block, "state": "failed" }),
                };
            }
            Err(e) => {
                return GatewayDispatchOutcome {
                    state: "failed",
                    error: Some(format!("GATEWAY_VALIDATE_FAILED: {e}")),
                    receipt: json!({ "block": block, "state": "failed", "reason": e }),
                };
            }
        }

        // 3. Dispatch ConfigApply and await terminal ConfigReceipt (Persisted -> Active).
        let operation_id = id::next("CFGOP");
        let mut rx = match hub.config_apply(ConfigApplyRequest {
            device: device.to_string(),
            core_node_id: device.to_string(),
            data_epoch,
            application_id: application_id.to_string(),
            operation_id: operation_id.clone(),
            desired_revision,
            block_generation: effective_gen,
            block,
            schema_version: 1,
            block_hash: block_hash.clone(),
            expected_gateway_boot_id: boot_id.clone(),
            expected_state_revision: state_rev,
            capability_fingerprint: cap_fp.clone(),
            values: values.clone(),
        }) {
            Ok(rx) => rx,
            Err(e) => {
                return GatewayDispatchOutcome {
                    state: "failed",
                    error: Some(format!("GATEWAY_APPLY_SEND_FAILED: {e}")),
                    receipt: json!({ "block": block, "state": "failed", "reason": e }),
                };
            }
        };

        let mut last_persisted_hash: Option<String> = None;
        let deadline = tokio::time::Instant::now() + DEFAULT_APPLY_TIMEOUT;
        let should_retry;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                hub.remove_config_apply_waiter(&operation_id);
                return GatewayDispatchOutcome {
                    state: "failed",
                    error: Some("GATEWAY_RECEIPT_TIMEOUT".into()),
                    receipt: json!({
                        "block": block,
                        "operation_id": operation_id,
                        "state": "failed",
                        "reason": "timeout awaiting terminal config_receipt",
                        "persisted_hash": last_persisted_hash,
                    }),
                };
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(GatewayFrame::ConfigReceipt {
                    state,
                    reason,
                    retryable,
                    gateway_boot_id,
                    state_revision,
                    persisted_hash,
                    active_hash,
                    active_values,
                    evidence,
                    ts,
                    ..
                })) => {
                    if persisted_hash.is_some() {
                        last_persisted_hash = persisted_hash.clone();
                    }
                    if !state.is_terminal() {
                        continue;
                    }
                    if state == ConfigReceiptStatus::Active {
                        return GatewayDispatchOutcome {
                            state: "applied",
                            error: None,
                            receipt: json!({
                                "block": block,
                                "operation_id": operation_id,
                                "block_generation": effective_gen,
                                "state": state,
                                "gateway_boot_id": gateway_boot_id,
                                "state_revision": state_revision,
                                "persisted_hash": last_persisted_hash,
                                "active_hash": active_hash,
                                "active_values": active_values,
                                "evidence": evidence,
                                "ts": ts,
                            }),
                        };
                    }
                    if retryable && attempt == 0 {
                        hub.remove_config_apply_waiter(&operation_id);
                        should_retry = true;
                        break;
                    }
                    hub.remove_config_apply_waiter(&operation_id);
                    let final_state = if state == ConfigReceiptStatus::Superseded {
                        "superseded"
                    } else {
                        "failed"
                    };
                    return GatewayDispatchOutcome {
                        state: final_state,
                        error: Some(
                            reason
                                .clone()
                                .unwrap_or_else(|| format!("GATEWAY_CONFIG_{state:?}")),
                        ),
                        receipt: json!({
                            "block": block,
                            "operation_id": operation_id,
                            "block_generation": effective_gen,
                            "state": state,
                            "reason": reason,
                            "retryable": retryable,
                            "gateway_boot_id": gateway_boot_id,
                            "state_revision": state_revision,
                            "persisted_hash": last_persisted_hash,
                            "active_hash": active_hash,
                            "active_values": active_values,
                            "evidence": evidence,
                            "ts": ts,
                        }),
                    };
                }
                Ok(Some(_)) => continue,
                Ok(None) => {
                    hub.remove_config_apply_waiter(&operation_id);
                    return GatewayDispatchOutcome {
                        state: "failed",
                        error: Some("GATEWAY_LINK_DROPPED".into()),
                        receipt: json!({
                            "block": block,
                            "operation_id": operation_id,
                            "state": "failed",
                            "reason": "waiter retired on link drop",
                        }),
                    };
                }
                Err(_) => {
                    hub.remove_config_apply_waiter(&operation_id);
                    return GatewayDispatchOutcome {
                        state: "failed",
                        error: Some("GATEWAY_RECEIPT_TIMEOUT".into()),
                        receipt: json!({
                            "block": block,
                            "operation_id": operation_id,
                            "state": "failed",
                            "reason": "timeout awaiting terminal config_receipt",
                            "persisted_hash": last_persisted_hash,
                        }),
                    };
                }
            }
        }
        if !should_retry {
            break;
        }
    }

    GatewayDispatchOutcome {
        state: "failed",
        error: Some("GATEWAY_DISPATCH_FAILED".into()),
        receipt: json!({ "block": block, "state": "failed" }),
    }
}

/// Dispatch one unit's `UnitAcousticProfile` over Gateway `config_apply_v2`
/// (`ConfigBlock::Acoustics`), using monotonic `revision * 10 + 1` generation
/// fencing compatible with `ConfigWorker`. Returns `saved_pending` honestly when
/// the device is offline or lacks v2 capability rather than inventing `"applied"`.
pub(crate) async fn dispatch_unit_acoustics_parts(
    store: &crate::store::Store,
    hub: &crate::gateway::DeviceHub,
    voice_settings: &crate::voice::VoiceSettings,
    device: &str,
    profile: &UnitAcousticProfile,
    is_new_write: bool,
) -> GatewayDispatchOutcome {
    if let Err(reason) = hub.supports_config_apply_v2(device, ConfigBlock::Acoustics) {
        return GatewayDispatchOutcome {
            state: "saved_pending",
            error: None,
            receipt: json!({
                "block": ConfigBlock::Acoustics,
                "state": "saved_pending",
                "reason": reason,
            }),
        };
    }

    let targets = match build_gateway_payload(
        "unit_acoustic_profiles",
        device,
        &json!({ device: profile }),
        None,
        Some(voice_settings),
        None,
        None,
        None,
    ) {
        Ok(t) => t,
        Err(e) => {
            return GatewayDispatchOutcome {
                state: "failed",
                error: Some(format!("{}: {}", e.code, e.detail)),
                receipt: json!({
                    "block": ConfigBlock::Acoustics,
                    "state": "failed",
                    "reason": e.detail,
                }),
            };
        }
    };
    let Some((block, values)) = targets.into_iter().next() else {
        return GatewayDispatchOutcome {
            state: "failed",
            error: Some("EMPTY_ACOUSTICS_PAYLOAD".into()),
            receipt: json!({ "block": ConfigBlock::Acoustics, "state": "failed" }),
        };
    };

    let (revision, data_epoch) = store
        .with(|c| {
            Ok(c.query_row(
                "SELECT revision, data_epoch FROM config_head WHERE id=1",
                [],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
            .unwrap_or((0, 0)))
        })
        .unwrap_or((0, 0));

    let desired_revision = if is_new_write {
        revision.saturating_add(1)
    } else {
        revision
    };
    let block_generation = desired_revision.saturating_mul(10).saturating_add(1);

    dispatch_block_v2_clamped(
        hub,
        device,
        data_epoch,
        &format!("device_profile_{}", id::next("AC")),
        desired_revision,
        block_generation,
        block,
        values,
    )
    .await
}

/// Convenience wrapper for API handlers writing a new `UnitAcousticProfile`.
pub(crate) async fn dispatch_unit_acoustics(
    state: &crate::api::AppState,
    device: &str,
    profile: &UnitAcousticProfile,
) -> GatewayDispatchOutcome {
    dispatch_unit_acoustics_parts(
        &state.store,
        &state.hub,
        &state.voice.settings_snapshot(),
        device,
        profile,
        true,
    )
    .await
}

fn read_config_head(store: &crate::store::Store) -> (u64, u64) {
    store
        .with(|c| {
            Ok(c.query_row(
                "SELECT revision, data_epoch FROM config_head WHERE id=1",
                [],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
            .unwrap_or((0, 0)))
        })
        .unwrap_or((0, 0))
}

/// Dispatch the saved `ReflexConfig` (keywords + KWS sensitivity) over Gateway
/// `config_apply_v2` (`ConfigBlock::Kws`). Returns `(ReflexDeploy, note_json,
/// api_response_json)` matching the legacy `deploy_reflex` contract shape.
pub(crate) async fn dispatch_reflex_v2(
    store: &std::sync::Arc<crate::store::Store>,
    hub: &crate::gateway::DeviceHub,
    settings: &crate::voice::VoiceSettings,
    device: &str,
) -> std::result::Result<(crate::voice::ReflexDeploy, Value, Value), String> {
    let cfg = crate::config::ConfigService::new(store.clone())
        .reflex()
        .map_err(|e| e.to_string())?;
    let compiled = compile_keywords(&cfg)?;
    let hash = crate::config::fnv1a_hex(compiled.as_bytes());
    let words = compiled.lines().count() as u64;
    let (retreat_words, intents_hash) = reflex_intent_map(&cfg);
    let kws_threshold = settings.kws_threshold;
    let kws_score = settings.kws_score;
    let values = compile_kws_block(&cfg, Some(kws_threshold), Some(kws_score), "reflex")
        .map_err(|e| e.detail)?;

    let (revision, data_epoch) = read_config_head(store);
    let desired_revision = revision;
    let block_generation = desired_revision.saturating_mul(10).saturating_add(1);
    let sent_ms = id::now_ms();
    let outcome = dispatch_block_v2_clamped(
        hub,
        device,
        data_epoch,
        &format!("reflex_deploy_{}", id::next("KW")),
        desired_revision,
        block_generation,
        ConfigBlock::Kws,
        values,
    )
    .await;
    if outcome.state != "applied" {
        return Err(outcome
            .error
            .unwrap_or_else(|| format!("config_apply_v2 kws {}", outcome.state)));
    }
    let acked_ms = id::now_ms();
    let rec = crate::voice::ReflexDeploy {
        state: "applied".into(),
        hash: hash.clone(),
        sent_hash: hash.clone(),
        loaded_hash: Some(hash.clone()),
        loaded_ms: acked_ms,
        keywords: words,
        detail: format!("applied {words} words via config_apply_v2"),
        sent_ms,
        acked_ms,
        retreat_words_sent: retreat_words.len() as u32,
        local_exit: retreat_words.len() as u32,
        intents_hash: intents_hash.clone(),
        intents_hash_installed: intents_hash.clone(),
        local_exits: 0,
        last_local_exit: None,
        kws_threshold,
        kws_score,
        applied_threshold: kws_threshold as f32,
        applied_score: kws_score as f32,
    };
    let note = json!({
        "device": device,
        "hash": hash,
        "words": words,
        "bytes": compiled.len(),
        "retreat_words": retreat_words,
        "intents_hash": intents_hash,
        "kws_threshold": kws_threshold,
        "kws_score": kws_score,
        "via": "config_apply_v2",
    });
    let out = json!({
        "state": "applied",
        "hash": hash,
        "words": words,
        "device": device,
        "keywords_file": compiled,
        "retreat_words": retreat_words,
        "intents_hash": intents_hash,
        "kws_threshold": kws_threshold,
        "kws_score": kws_score,
        "via": "config_apply_v2",
        "receipt": outcome.receipt,
    });
    Ok((rec, note, out))
}

/// Dispatch reconnect `duplex_requested` intent over Gateway `config_apply_v2`
/// (`ConfigBlock::Duplex`).
pub(crate) async fn dispatch_duplex_v2(
    store: &crate::store::Store,
    hub: &crate::gateway::DeviceHub,
    device: &str,
    requested: &str,
) -> GatewayDispatchOutcome {
    let (revision, data_epoch) = read_config_head(store);
    let block_generation = revision.saturating_mul(10).saturating_add(1);
    dispatch_block_v2_clamped(
        hub,
        device,
        data_epoch,
        &format!("reconnect_duplex_{}", id::next("DX")),
        revision,
        block_generation,
        ConfigBlock::Duplex,
        json!({ "duplex_requested": requested }),
    )
    .await
}

/// Dispatch reconnect `AudioEqConfig` over Gateway `config_apply_v2`
/// (`ConfigBlock::AudioEq`), reusing [`build_gateway_payload`].
pub(crate) async fn dispatch_audio_eq_v2(
    store: &std::sync::Arc<crate::store::Store>,
    hub: &crate::gateway::DeviceHub,
    device: &str,
) -> GatewayDispatchOutcome {
    let cfg = crate::config::ConfigService::new(store.clone())
        .audio_eq()
        .unwrap_or_default();
    let desired = serde_json::to_value(&cfg).unwrap_or_else(|_| json!({}));
    let targets =
        match build_gateway_payload("audio_eq", device, &desired, None, None, None, None, None) {
            Ok(t) => t,
            Err(e) => {
                return GatewayDispatchOutcome {
                    state: "failed",
                    error: Some(format!("{}: {}", e.code, e.detail)),
                    receipt: json!({
                        "block": ConfigBlock::AudioEq,
                        "state": "failed",
                        "reason": e.detail,
                    }),
                };
            }
        };
    let Some((block, values)) = targets.into_iter().next() else {
        return GatewayDispatchOutcome {
            state: "failed",
            error: Some("EMPTY_AUDIO_EQ_PAYLOAD".into()),
            receipt: json!({ "block": ConfigBlock::AudioEq, "state": "failed" }),
        };
    };
    let (revision, data_epoch) = read_config_head(store);
    let block_generation = revision.saturating_mul(10).saturating_add(1);
    dispatch_block_v2_clamped(
        hub,
        device,
        data_epoch,
        &format!("reconnect_audio_eq_{}", id::next("EQ")),
        revision,
        block_generation,
        block,
        values,
    )
    .await
}
