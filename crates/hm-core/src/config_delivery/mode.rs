use super::{
    service::{raw, value_tree},
    ConfigDelivery, DeliveryError, Fields, Layers, Result,
};
use crate::{
    store::{self, Store},
    voice::{FollowWindowPatch, VoiceSettings, VOICE_CFG_KEY},
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};

impl ConfigDelivery {
    pub(crate) fn save_voice_settings_edit(
        store: &Store,
        before: &VoiceSettings,
        settings: &VoiceSettings,
        actor: &str,
        summary: &str,
    ) -> store::Result<()> {
        store.with(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let before = serde_json::to_value(before)?;
            let after = serde_json::to_value(settings)?;
            let mut persisted = stored_voice(&tx)?.unwrap_or_else(|| before.clone());
            if !persisted.is_object() {
                return Err(store::StoreError::Authority(
                    "invalid saved voice settings".into(),
                ));
            }
            if let Some(changes) = changed_values(&before, &after) {
                super::service::merge_known(&mut persisted, &changes);
            }
            if before["duplex_requested"] != after["duplex_requested"] {
                persisted["duplex_requested"] = after["duplex_requested"].clone();
            }
            persist_payload(&tx, &persisted.to_string(), actor, summary)?;
            tx.commit()?;
            Ok(())
        })
    }

    pub(crate) fn save_voice_settings_snapshot(
        store: &Store,
        settings: &VoiceSettings,
        document: Option<&Value>,
        actor: &str,
        summary: &str,
    ) -> store::Result<()> {
        store.with(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let canonical = serde_json::to_value(settings)?;
            let mut opaque = if let Some(document) = document {
                document.clone()
            } else {
                stored_voice(&tx)?
                    .filter(Value::is_object)
                    .unwrap_or_else(|| canonical.clone())
            };
            super::service::merge_known(&mut opaque, &canonical);
            opaque["duplex_requested"] = canonical["duplex_requested"].clone();
            persist_payload(&tx, &opaque.to_string(), actor, summary)?;
            tx.commit()?;
            Ok(())
        })
    }
}

fn stored_voice(c: &Connection) -> store::Result<Option<Value>> {
    let raw: Option<String> = c
        .query_row(
            "SELECT value FROM config_kv WHERE key=?1",
            [VOICE_CFG_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.as_deref().map(serde_json::from_str).transpose()?)
}

fn persist_payload(c: &Connection, payload: &str, actor: &str, summary: &str) -> store::Result<()> {
    c.execute(
        "INSERT INTO config_kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![VOICE_CFG_KEY, payload],
    )?;
    c.execute(
        "INSERT INTO config_history(ts_ms,actor,scope,summary,payload) VALUES(?1,?2,'voice',?3,?4)",
        params![crate::id::now_ms() as i64, actor, summary, payload],
    )?;
    Ok(())
}

impl ConfigDelivery {
    pub(crate) fn save_voice_settings_patch(
        store: &Store,
        settings: &VoiceSettings,
        patch: &Value,
        actor: &str,
        summary: &str,
    ) -> store::Result<()> {
        store.with(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let mut persisted =
                stored_voice(&tx)?.unwrap_or(serde_json::to_value(VoiceSettings::default())?);
            if !persisted.is_object() {
                return Err(store::StoreError::Authority(
                    "invalid saved voice settings".into(),
                ));
            }
            let canonical = projection(settings);
            let selected = selected_values(patch, &canonical);
            super::service::merge_known(&mut persisted, &selected);
            persist_payload(&tx, &persisted.to_string(), actor, summary)?;
            tx.commit()?;
            Ok(())
        })
    }

    pub(crate) fn patch_voice_settings(
        current: &VoiceSettings,
        patch: &Value,
    ) -> Result<VoiceSettings> {
        check_scope(patch, &projection(current), "mode")?;
        let mut merged = serde_json::to_value(current)?;
        super::service::merge_known(&mut merged, patch);
        let next: VoiceSettings = serde_json::from_value(merged)?;
        next.validate_reviewed_delivery().map_err(delivery_error)?;
        Ok(next)
    }
}

/// Nested leaves are selected independently so legacy private fields are
/// retained when an editor changes a single reviewed delivery limit.
fn selected_values(mask: &Value, canonical: &Value) -> Value {
    match (mask.as_object(), canonical.as_object()) {
        (Some(mask), Some(values)) => Value::Object(
            mask.iter()
                .filter_map(|(key, scope)| {
                    values
                        .get(key)
                        .map(|value| (key.clone(), selected_values(scope, value)))
                })
                .collect(),
        ),
        _ => canonical.clone(),
    }
}

fn changed_values(before: &Value, after: &Value) -> Option<Value> {
    if before == after {
        return None;
    }
    Some(match (before.as_object(), after.as_object()) {
        (Some(before), Some(after)) => Value::Object(
            after
                .iter()
                .filter_map(|(key, value)| {
                    changed_values(before.get(key).unwrap_or(&Value::Null), value)
                        .map(|value| (key.clone(), value))
                })
                .collect(),
        ),
        _ => after.clone(),
    })
}

pub(super) fn check_scope(request: &Value, allowed: &Value, path: &str) -> Result<()> {
    let values = request.as_object().ok_or_else(|| {
        DeliveryError::new("INVALID_VOICE_POLICY", "expected a voice policy object").at(path)
    })?;
    if path == "mode.duplex_requested" {
        for (key, value) in values {
            let child = format!("{path}.{key}");
            if !matches!(value.as_str(), Some("half" | "full")) {
                return Err(DeliveryError::new(
                    "INVALID_VOICE_POLICY",
                    "duplex_requested must be 'half' or 'full'",
                )
                .at(&child));
            }
        }
        return Ok(());
    }
    for (key, value) in values {
        let child = format!("{path}.{key}");
        let rule = allowed.get(key).ok_or_else(|| {
            DeliveryError::new(
                "PROTECTED_FIELD",
                "field belongs to another voice or device operation",
            )
            .at(&child)
        })?;
        if rule.is_object() {
            check_scope(value, rule, &child)?;
        }
    }
    Ok(())
}

fn delivery_error(error: String) -> DeliveryError {
    DeliveryError::new("INVALID_VOICE_POLICY", error).at("mode")
}

pub(super) fn write_projection_on(
    c: &Connection,
    current: &VoiceSettings,
    desired: &Value,
    actor: &str,
    summary: &str,
) -> Result<()> {
    let mut persisted = raw(c, VOICE_CFG_KEY)?
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(serde_json::to_value(VoiceSettings::default())?);
    if let Some(changes) = changed_values(&projection(current), desired) {
        super::service::merge_known(&mut persisted, &changes);
    }
    persist_payload(c, &persisted.to_string(), actor, summary).map_err(DeliveryError::internal)
}

/// The reviewed Core policy excludes device negotiation and unit acoustics.
pub(super) fn projection(settings: &VoiceSettings) -> Value {
    json!({
        "assistant_name": settings.assistant_name,
        "persona_prompt": settings.persona_prompt,
        "system_prompt_override": settings.system_prompt_override,
        "follow_window_enabled": settings.follow_window_enabled,
        "follow_window_ms": settings.follow_window_ms,
        "context_window_ms": settings.context_window_ms,
        "followup_credits": settings.followup_credits,
        "follow_window_tone": settings.follow_window_tone,
        "wake_words": settings.wake_words,
        "kws_threshold": settings.kws_threshold,
        "kws_score": settings.kws_score,
        "duplex_requested": settings.duplex_requested,
        "retreat_intent": settings.retreat_intent,
        "retreat_words": settings.retreat_words,
        "retreat_ack": settings.retreat_ack,
        "window_close_ack": settings.window_close_ack,
        "truncation_notice": settings.truncation_notice,
        "wake_ack": {
            "enabled": settings.wake_ack.enabled,
            "phrases": settings.wake_ack.phrases,
        },
        "filler": {
            "delay_ms": settings.filler.delay_ms,
            "tool_delay_ms": settings.filler.tool_delay_ms,
            "mode": settings.filler.mode,
            "thinking": settings.filler.thinking,
            "search": settings.filler.search,
            "device": settings.filler.device,
            "story": settings.filler.story,
        },
        "stop_intents": settings.stop_intents,
        "spoken": {
            "max_sentences": settings.spoken.max_sentences,
            "max_chars": settings.spoken.max_chars,
            "detail_max_sentences": settings.spoken.detail_max_sentences,
            "detail_max_chars": settings.spoken.detail_max_chars,
            "continue_hint": settings.spoken.continue_hint,
            "resume_lead": settings.spoken.resume_lead,
            "backstop_max_sentences": settings.spoken.backstop_max_sentences,
            "backstop_max_chars": settings.spoken.backstop_max_chars,
        },
    })
}

pub(super) fn native(c: &Connection) -> Result<VoiceSettings> {
    Ok(raw(c, VOICE_CFG_KEY)?
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}

pub(super) fn resolve_projection(
    current: &VoiceSettings,
    desired: &Value,
) -> Result<VoiceSettings> {
    resolve_projection_with(current, desired, true)
}

fn resolve_projection_with(
    current: &VoiceSettings,
    desired: &Value,
    combined: bool,
) -> Result<VoiceSettings> {
    let mut full = serde_json::to_value(current)?;
    check_scope(desired, &projection(current), "mode")?;
    super::service::merge_known(&mut full, desired);
    let next: VoiceSettings = serde_json::from_value(full)?;
    if combined {
        next.validate_reviewed_delivery().map_err(delivery_error)?;
    } else {
        next.validate_delivery_policy_values()
            .map_err(delivery_error)?;
    }
    FollowWindowPatch {
        follow_window_enabled: Some(next.follow_window_enabled),
        follow_window_ms: Some(next.follow_window_ms),
        context_window_ms: Some(next.context_window_ms),
        followup_credits: next.followup_credits,
        assistant_name: Some(next.assistant_name.clone()),
        persona_prompt: Some(next.persona_prompt.clone()),
        system_prompt_override: Some(next.system_prompt_override.clone()),
        ..Default::default()
    }
    .validate()
    .map_err(|error| DeliveryError::new("INVALID_VOICE_POLICY", error).at("mode"))?;
    if next.assistant_name.trim() != next.assistant_name
        || next.persona_prompt.trim() != next.persona_prompt
        || next
            .system_prompt_override
            .as_deref()
            .is_some_and(|text| text.is_empty() || text.trim() != text)
    {
        return Err(DeliveryError::new(
            "INVALID_VOICE_POLICY",
            "voice prompt values must use canonical spacing",
        )
        .at("mode"));
    }
    Ok(next)
}

pub(super) fn review_layers(layers: &Layers, resolved: &Fields) -> Result<()> {
    let defaults = VoiceSettings::default();
    resolve_projection(&defaults, &value_tree(&layers.family, "mode"))?;
    resolve_projection_with(&defaults, &value_tree(&layers.node, "mode"), false)?;
    resolve_projection(&defaults, &value_tree(resolved, "mode"))?;
    Ok(())
}

pub(super) fn published(c: &Connection) -> Result<Option<Value>> {
    if raw(c, VOICE_CFG_KEY)?.is_none() {
        return Ok(None);
    }
    let current = match native(c) {
        Ok(current) => current,
        Err(_) => return Ok(None),
    };
    let observed = projection(&current);
    Ok(resolve_projection(&current, &observed)
        .is_ok()
        .then_some(observed))
}

pub(super) fn require_readable(c: &Connection) -> Result<()> {
    if raw(c, VOICE_CFG_KEY)?.is_some() && published(c)?.is_none() {
        return Err(DeliveryError::conflict(
            "VOICE_BASELINE_UNREADABLE",
            "repair the active voice settings before reviewing a package",
        )
        .at("mode"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_cue_projection_validates_and_guards_resynth() {
        let base = VoiceSettings::default();
        let proj = projection(&base);
        for key in [
            "retreat_ack",
            "window_close_ack",
            "truncation_notice",
            "wake_ack",
            "filler",
        ] {
            assert!(proj.get(key).is_some(), "missing {key} in projection");
        }
        assert!(proj["wake_ack"].get("resynth").is_none());
        assert!(proj["filler"].get("resynth").is_none());

        let valid = json!({
            "wake_ack": {"enabled": false, "phrases": ["在呢", "请讲"]},
            "retreat_ack": "好滴",
            "window_close_ack": "再会",
            "truncation_notice": "本次内容先讲到这里。",
            "filler": {
                "mode": "interrupt",
                "delay_ms": 450,
                "tool_delay_ms": 900,
                "thinking": ["让我想想。"],
                "search": ["我去查查。"],
                "device": ["正在操作。"],
                "story": ["我想想怎么编。"]
            }
        });
        let updated = ConfigDelivery::patch_voice_settings(&base, &valid).unwrap();
        assert!(!updated.wake_ack.enabled);
        assert_eq!(updated.wake_ack.phrases, vec!["在呢", "请讲"]);
        assert_eq!(updated.retreat_ack, "好滴");
        assert_eq!(updated.window_close_ack, "再会");
        assert_eq!(updated.truncation_notice, "本次内容先讲到这里。");
        assert_eq!(updated.filler.delay_ms, 450);

        for bad in [
            json!({"wake_ack": {"resynth": true}}),
            json!({"filler": {"resynth": true}}),
            json!({"retreat_ack": ""}),
            json!({"retreat_ack": "超过八个字肯定不行"}),
            json!({"window_close_ack": "bad/ack"}),
            json!({"wake_ack": {"phrases": []}}),
            json!({"wake_ack": {"phrases": ["一二三四五六七"]}}),
            json!({"truncation_notice": ""}),
            json!({"truncation_notice": "   "}),
            json!({"filler": {"delay_ms": 100}}),
            json!({"filler": {"thinking": ["重复", "重复"]}}),
        ] {
            assert!(
                ConfigDelivery::patch_voice_settings(&base, &bad).is_err(),
                "expected error for {bad}"
            );
        }
    }
}
