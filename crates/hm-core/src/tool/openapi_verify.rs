//! Helpers for OpenAPI write-operation request bodies, JSON path extraction,
//! and read-back argument forwarding.

use crate::config::{
    EffectLevel, OpenApiOperation, OpenApiParam, OpenApiParamLocation, VerificationStrategy,
};
use crate::tool::openapi::{
    OpenApiRuntime, OPENAPI_CONFIRMATION_REQUIRED, OPENAPI_READBACK_FAILED,
    OPENAPI_READBACK_MISMATCH, OPENAPI_RESPONSE_ASSERT_FAILED,
};
use crate::tool::{Evidence, FactBasis, ToolError, ToolFact, ToolResult, ToolStatus};
use serde_json::{json, Map, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

impl OpenApiRuntime {
    /// Execute a whitelisted OpenAPI operation with confirmation gating and
    /// per-operation verification (`ReadbackOp`, `ResponseAssert`, `TransportOnly`).
    pub async fn execute_operation(
        &self,
        operation: &str,
        args: &Value,
        confirmed: bool,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> ToolResult {
        // Compatibility signature retains no boolean authority. All direct
        // callers must use non-destructive operations; model JSON cannot approve.
        let _ = confirmed;
        match self.prepare_operation(operation, args) {
            Ok(prepared) => self.execute_snapshot(prepared, cancel).await,
            Err((e, _)) => ToolResult::failed(e.code, e.detail, false),
        }
    }
    pub(super) async fn execute_snapshot(
        &self,
        prepared: crate::tool::openapi::PreparedOperation,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> ToolResult {
        if prepared.op.effect_level == EffectLevel::Destructive {
            return ToolResult::failed(
                OPENAPI_CONFIRMATION_REQUIRED,
                "paired approval and separate execution required",
                false,
            );
        }
        self.execute_admitted(prepared, cancel, None).await
    }
    pub(crate) async fn dispatch_confirmation(
        &self,
        prepared: crate::tool::openapi::PreparedOperation,
        permit: crate::confirmation::DispatchPermit,
    ) -> ToolResult {
        self.execute_admitted(prepared, None, Some(permit)).await
    }
    async fn execute_admitted(
        &self,
        prepared: crate::tool::openapi::PreparedOperation,
        cancel: Option<&Arc<AtomicBool>>,
        permit: Option<crate::confirmation::DispatchPermit>,
    ) -> ToolResult {
        let secrets = prepared.secrets.clone();
        let mut result = self.execute_frozen(prepared, cancel, permit).await;
        crate::tool::openapi::scrub_result(&mut result, &secrets);
        result
    }
    async fn execute_frozen(
        &self,
        prepared: crate::tool::openapi::PreparedOperation,
        cancel: Option<&Arc<AtomicBool>>,
        permit: Option<crate::confirmation::DispatchPermit>,
    ) -> ToolResult {
        let crate::tool::openapi::PreparedOperation {
            cfg,
            op,
            request,
            readback,
            token,
            ..
        } = prepared;
        let is_write =
            !op.method.eq_ignore_ascii_case("get") || op.effect_level != EffectLevel::Query;
        let destructive = op.effect_level == EffectLevel::Destructive;
        let (mut write_out, write_reqs) = match self
            .send_http(&cfg, &op, request, token.as_deref(), cancel, permit)
            .await
        {
            Ok(value) => value,
            Err((e, requests)) => {
                let mut result = ToolResult::failed(
                    e.code,
                    e.detail.clone(),
                    if destructive { false } else { e.retryable },
                );
                result.result = Some(json!({"http_requests":requests}));
                if destructive && requests > 0 {
                    result.status = ToolStatus::Unknown;
                    result.evidence = Evidence::Sent;
                }
                if is_write {
                    result.fact = Some(ToolFact {
                        subject: op.id.clone(),
                        action: "other".into(),
                        basis: FactBasis::ReadBack,
                        confirmed: false,
                        value: None,
                        detail: Some(e.detail),
                    });
                }
                return result;
            }
        };
        if !is_write {
            if let Some(o) = write_out.as_object_mut() {
                o.insert("http_requests".into(), json!(write_reqs));
            }
            return ToolResult::observed_ok(write_out);
        }
        let strategy = op
            .verification
            .as_ref()
            .unwrap_or(&VerificationStrategy::TransportOnly);
        match strategy {
            VerificationStrategy::TransportOnly => {
                if let Some(o) = write_out.as_object_mut() {
                    o.insert("http_requests".into(), json!(write_reqs));
                    o.insert("verification".into(), json!("transport_only"));
                    o.insert("note".into(), json!("已发送请求，未验证最终状态"));
                }
                ToolResult {
                    status: ToolStatus::Completed,
                    evidence: Evidence::Sent,
                    result: Some(write_out),
                    error: None,
                    fact: Some(ToolFact {
                        subject: op.id.clone(),
                        action: "other".into(),
                        basis: FactBasis::ReadBack,
                        confirmed: false,
                        value: None,
                        detail: Some("已发送请求，未验证最终状态".into()),
                    }),
                }
            }
            VerificationStrategy::ResponseAssert {
                json_path,
                expected,
            } => {
                if let Some(o) = write_out.as_object_mut() {
                    o.insert("http_requests".into(), json!(write_reqs));
                    o.insert("verification".into(), json!("response_assert"));
                }
                let actual = write_out
                    .pointer("/body/json")
                    .and_then(|j| extract_json_path(j, json_path));
                if actual == Some(expected) {
                    ToolResult {
                        status: ToolStatus::Completed,
                        evidence: Evidence::Observed,
                        result: Some(write_out),
                        error: None,
                        fact: Some(ToolFact {
                            subject: op.id.clone(),
                            action: "other".into(),
                            basis: FactBasis::ReadBack,
                            confirmed: true,
                            value: None,
                            detail: None,
                        }),
                    }
                } else {
                    let detail = format!("响应断言未通过：{}；敏感值不显示", json_path);
                    ToolResult {
                        status: ToolStatus::Failed,
                        evidence: Evidence::Sent,
                        result: Some(write_out),
                        error: Some(ToolError {
                            code: OPENAPI_RESPONSE_ASSERT_FAILED.into(),
                            retryable: false,
                            detail,
                        }),
                        fact: Some(ToolFact {
                            subject: op.id.clone(),
                            action: "other".into(),
                            basis: FactBasis::ReadBack,
                            confirmed: false,
                            value: None,
                            detail: Some("响应断言未通过".into()),
                        }),
                    }
                }
            }
            VerificationStrategy::ReadbackOp {
                op_id,
                json_path,
                expected,
            } => {
                let Some(rb_op) = cfg.find_operation(op_id).filter(|o| {
                    o.method.eq_ignore_ascii_case("get") && o.effect_level == EffectLevel::Query
                }) else {
                    return ToolResult {
                        status: ToolStatus::Failed,
                        evidence: Evidence::Sent,
                        result: Some(json!({
                            "write": write_out,
                            "http_requests": write_reqs,
                        })),
                        error: Some(ToolError {
                            code: OPENAPI_READBACK_FAILED.into(),
                            retryable: false,
                            detail: format!("回读操作 '{op_id}' 不存在或不是 GET 操作"),
                        }),
                        fact: Some(ToolFact {
                            subject: op.id.clone(),
                            action: "other".into(),
                            basis: FactBasis::ReadBack,
                            confirmed: false,
                            value: None,
                            detail: Some("回读配置无效".into()),
                        }),
                    };
                };
                // Query readback arguments/target were validated and frozen
                // before approval; never reconstruct them from mutable inputs.
                let Some(readback) = readback else {
                    return ToolResult::failed(
                        OPENAPI_READBACK_FAILED,
                        "missing prepared query readback",
                        false,
                    );
                };
                match self
                    .send_http(&cfg, rb_op, readback, token.as_deref(), cancel, None)
                    .await
                {
                    Ok((rb_out, rb_reqs)) => {
                        let total_reqs = write_reqs + rb_reqs;
                        let actual = rb_out
                            .pointer("/body/json")
                            .and_then(|j| extract_json_path(j, json_path));
                        let combined = json!({
                            "operation": op.id,
                            "status": write_out.get("status"),
                            "body": write_out.get("body"),
                            "readback": rb_out,
                            "verification": "readback_op",
                            "http_requests": total_reqs,
                        });
                        if actual == Some(expected) {
                            ToolResult {
                                status: ToolStatus::Completed,
                                evidence: Evidence::Observed,
                                result: Some(combined),
                                error: None,
                                fact: Some(ToolFact {
                                    subject: op.id.clone(),
                                    action: "other".into(),
                                    basis: FactBasis::ReadBack,
                                    confirmed: true,
                                    value: None,
                                    detail: None,
                                }),
                            }
                        } else {
                            ToolResult {
                                status: ToolStatus::Failed,
                                evidence: Evidence::Sent,
                                result: Some(combined),
                                error: Some(ToolError {
                                    code: OPENAPI_READBACK_MISMATCH.into(),
                                    retryable: false,
                                    detail: format!("回读验证未通过：{}；敏感值不显示", json_path),
                                }),
                                fact: Some(ToolFact {
                                    subject: op.id.clone(),
                                    action: "other".into(),
                                    basis: FactBasis::ReadBack,
                                    confirmed: false,
                                    value: None,
                                    detail: Some("回读状态不匹配".into()),
                                }),
                            }
                        }
                    }
                    Err((e, rb_reqs)) => ToolResult {
                        status: ToolStatus::Failed,
                        evidence: Evidence::Sent,
                        result: Some(json!({
                            "write": write_out,
                            "http_requests": write_reqs + rb_reqs,
                        })),
                        error: Some(ToolError {
                            code: OPENAPI_READBACK_FAILED.into(),
                            retryable: false,
                            detail: format!("回读操作 '{}' 失败：{}", rb_op.id, e.detail),
                        }),
                        fact: Some(ToolFact {
                            subject: op.id.clone(),
                            action: "other".into(),
                            basis: FactBasis::ReadBack,
                            confirmed: false,
                            value: None,
                            detail: Some("回读请求失败".into()),
                        }),
                    },
                }
            }
        }
    }
}

/// Extract a value from a JSON document using a simple dot/slash/bracket path
/// (e.g. `"state"`, `"data.power"`, `"$.items[0].status"`, `"/data/enabled"`).
pub fn extract_json_path<'a>(val: &'a Value, path: &str) -> Option<&'a Value> {
    let trimmed = path.trim();
    let trimmed = trimmed
        .strip_prefix("$.")
        .or_else(|| trimmed.strip_prefix('$'))
        .unwrap_or(trimmed);
    let trimmed = trimmed.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(val);
    }

    let mut cur = val;
    for raw_seg in trimmed.split(['.', '/']).filter(|s| !s.is_empty()) {
        // Handle segments like `items[0]` or `[0]`
        let mut rest = raw_seg;
        while !rest.is_empty() {
            if let Some(bracket) = rest.find('[') {
                let key = &rest[..bracket];
                if !key.is_empty() {
                    cur = cur.get(key)?;
                }
                let after = &rest[bracket + 1..];
                let end = after.find(']')?;
                let idx_str = &after[..end];
                let idx: usize = idx_str.parse().ok()?;
                cur = cur.get(idx)?;
                rest = &after[end + 1..];
            } else {
                cur = match cur {
                    Value::Object(map) => map.get(rest)?,
                    Value::Array(arr) => {
                        let idx: usize = rest.parse().ok()?;
                        arr.get(idx)?
                    }
                    _ => return None,
                };
                break;
            }
        }
    }
    Some(cur)
}

/// Forward matching declared parameters from the write call's `args` to the
/// follow-up `ReadbackOp` GET operation.
pub fn build_readback_args(args: &Value, readback_op: &OpenApiOperation) -> Value {
    let mut out = Map::new();
    if let Some(obj) = args.as_object() {
        for p in &readback_op.params {
            if let Some(v) = obj.get(&p.name) {
                out.insert(p.name.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Validate and normalize a JSON body property value according to `OpenApiParam`.
pub fn normalize_body_param(p: &OpenApiParam, value: &Value) -> Result<Value, String> {
    match p.kind.as_str() {
        "string" => {
            let s = value
                .as_str()
                .ok_or_else(|| format!("参数 '{}' 需要字符串", p.name))?;
            if !p.enum_values.is_empty() && !p.enum_values.iter().any(|e| e == s) {
                return Err(format!(
                    "参数 '{}' 只能是 {}",
                    p.name,
                    p.enum_values.join(" | ")
                ));
            }
            Ok(Value::String(s.to_string()))
        }
        "integer" => {
            let i = if let Some(i) = value.as_i64() {
                i
            } else if let Some(f) = value.as_f64().filter(|f| {
                f.fract() == 0.0 && f.is_finite() && *f >= i64::MIN as f64 && *f < (i64::MAX as f64)
            }) {
                f as i64
            } else {
                return Err(format!("参数 '{}' 需要整数", p.name));
            };
            let s = i.to_string();
            if !p.enum_values.is_empty() && !p.enum_values.contains(&s) {
                return Err(format!(
                    "参数 '{}' 只能是 {}",
                    p.name,
                    p.enum_values.join(" | ")
                ));
            }
            Ok(Value::Number(i.into()))
        }
        "number" => {
            let f = value
                .as_f64()
                .filter(|f| f.is_finite())
                .ok_or_else(|| format!("参数 '{}' 需要数字", p.name))?;
            let s = if f.fract() == 0.0 {
                format!("{}", f as i64)
            } else {
                f.to_string()
            };
            if !p.enum_values.is_empty() && !p.enum_values.contains(&s) {
                return Err(format!(
                    "参数 '{}' 只能是 {}",
                    p.name,
                    p.enum_values.join(" | ")
                ));
            }
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| format!("参数 '{}' 需要有效数字", p.name))
        }
        "boolean" => {
            let b = value
                .as_bool()
                .ok_or_else(|| format!("参数 '{}' 需要 true/false", p.name))?;
            let s = b.to_string();
            if !p.enum_values.is_empty() && !p.enum_values.contains(&s) {
                return Err(format!(
                    "参数 '{}' 只能是 {}",
                    p.name,
                    p.enum_values.join(" | ")
                ));
            }
            Ok(Value::Bool(b))
        }
        other => Err(format!("参数 '{}' 的类型 '{other}' 不支持", p.name)),
    }
}

/// Only local `#/components/...` references are resolved.
pub fn resolve_local_ref<'a>(doc: &'a Value, value: &'a Value) -> Option<&'a Value> {
    let Some(r) = value.get("$ref").and_then(Value::as_str) else {
        return Some(value);
    };
    let path = r.strip_prefix("#/")?;
    let mut cur = doc;
    for part in path.split('/') {
        let part = part.replace("~1", "/").replace("~0", "~");
        cur = cur.get(&part)?;
    }
    Some(cur)
}

/// Resolve a schema through local `$ref`s up to 4 hops.
pub fn resolve_local_schema<'a>(doc: &'a Value, schema: &'a Value) -> Option<&'a Value> {
    let mut cur = schema;
    for _ in 0..4 {
        if cur.get("$ref").is_none() {
            return Some(cur);
        }
        cur = resolve_local_ref(doc, cur)?;
    }
    None
}

/// Extract `application/json` object properties from an OpenAPI 3 `requestBody`.
pub fn parse_request_body_params(
    doc: &Value,
    op: &Map<String, Value>,
    params: &mut Vec<OpenApiParam>,
    seen: &mut Vec<(String, OpenApiParamLocation)>,
    reason: &mut Option<String>,
) {
    let Some(raw_rb) = op.get("requestBody") else {
        return;
    };
    let Some(rb) = resolve_local_ref(doc, raw_rb).and_then(Value::as_object) else {
        reason.get_or_insert_with(|| "requestBody 使用无法解析的 $ref".into());
        return;
    };
    let rb_required = rb.get("required").and_then(Value::as_bool).unwrap_or(false);
    let Some(json_media) = rb
        .get("content")
        .and_then(|c| c.get("application/json"))
        .and_then(|m| m.get("schema"))
    else {
        if rb_required {
            reason.get_or_insert_with(|| "requestBody 仅支持 application/json 对象".into());
        }
        return;
    };
    let Some(schema) = resolve_local_schema(doc, json_media).and_then(Value::as_object) else {
        reason.get_or_insert_with(|| "requestBody schema 使用无法解析的 $ref".into());
        return;
    };
    let required_fields: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        if rb_required {
            reason.get_or_insert_with(|| "requestBody 缺少 properties 对象".into());
        }
        return;
    };
    for (prop_name, raw_prop) in props {
        let Some(prop_schema) = resolve_local_schema(doc, raw_prop) else {
            if required_fields.iter().any(|r| r == prop_name) {
                reason
                    .get_or_insert_with(|| format!("body 字段 '{prop_name}' 使用无法解析的 $ref"));
            }
            continue;
        };
        let enum_values = prop_schema
            .get("enum")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        Value::Bool(b) => Some(b.to_string()),
                        _ => None,
                    })
                    .take(32)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let declared = prop_schema.get("type").and_then(Value::as_str);
        let kind = match declared {
            Some(k @ ("string" | "integer" | "number" | "boolean")) => k.to_string(),
            None if !enum_values.is_empty() => "string".to_string(),
            other => {
                if required_fields.iter().any(|r| r == prop_name) {
                    let shown = other.unwrap_or("（没有 type）");
                    reason.get_or_insert_with(|| {
                        format!("body 字段 '{prop_name}' 的类型 '{shown}' 不支持")
                    });
                }
                continue;
            }
        };
        let key = (prop_name.clone(), OpenApiParamLocation::Body);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        params.push(OpenApiParam {
            sensitive: prop_schema.get("format").and_then(Value::as_str) == Some("password")
                || prop_schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
                || prop_schema.get("x-sensitive").and_then(Value::as_bool) == Some(true),
            name: prop_name.clone(),
            location: OpenApiParamLocation::Body,
            required: required_fields.iter().any(|r| r == prop_name),
            kind,
            enum_values,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_json_path_handles_dot_dollar_slash_and_array() {
        let v = json!({
            "status": "on",
            "data": { "power": true, "items": [ { "name": "lamp" }, { "name": "fan" } ] }
        });
        assert_eq!(extract_json_path(&v, "status"), Some(&json!("on")));
        assert_eq!(extract_json_path(&v, "$.data.power"), Some(&json!(true)));
        assert_eq!(extract_json_path(&v, "/data/power"), Some(&json!(true)));
        assert_eq!(
            extract_json_path(&v, "data.items[1].name"),
            Some(&json!("fan"))
        );
        assert_eq!(
            extract_json_path(&v, "data.items.0.name"),
            Some(&json!("lamp"))
        );
        assert_eq!(extract_json_path(&v, "data.missing"), None);
    }
}
