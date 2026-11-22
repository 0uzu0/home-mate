//! Immutable validated request snapshots; private payloads never enter previews verbatim.
use super::*;
use sha2::{Digest, Sha256};
pub(crate) struct PreparedHttp {
    pub(crate) method: reqwest::Method,
    pub(crate) url: String,
    pub(crate) body_obj: serde_json::Map<String, Value>,
    pub(crate) retry_post: bool,
    pub(crate) arguments: Value,
}
pub(crate) struct PreparedOperation {
    pub(crate) cfg: OpenApiConfig,
    pub(crate) op: OpenApiOperation,
    pub(crate) request: PreparedHttp,
    pub(crate) readback: Option<PreparedHttp>,
    pub(crate) token: Option<String>,
    pub(crate) revision: String,
    pub(crate) digest: String,
    pub(crate) preview: Value,
    pub(crate) args: Value,
    pub(crate) secrets: Vec<Value>,
}
impl OpenApiRuntime {
    pub(super) fn public_arguments(&self, args: &Value) -> Value {
        let snapshot = self.snapshot();
        let mut secrets = snapshot
            .token
            .map(|token| vec![json!(token)])
            .unwrap_or_default();
        let cfg = snapshot.config;
        let declared =
            cfg.find_operation(args.get("operation").and_then(Value::as_str).unwrap_or(""));
        if let Some(arguments) = args.get("arguments").and_then(Value::as_object) {
            for (name, value) in arguments {
                if sensitive(name)
                    || declared
                        .is_some_and(|op| op.params.iter().any(|p| p.name == *name && p.sensitive))
                {
                    secrets.push(value.clone());
                }
            }
        }
        let mut public = args.clone();
        if let Some(op) = declared {
            if let Some(arguments) = public.get_mut("arguments").and_then(Value::as_object_mut) {
                for param in op.params.iter().filter(|p| p.sensitive) {
                    if let Some(value) = arguments.get_mut(&param.name) {
                        *value = json!("[redacted]");
                    }
                }
            }
        }
        scrub_value(&mut public, &secrets);
        public
    }
    pub(crate) fn attach_confirmations(
        &self,
        service: &Arc<crate::confirmation::ConfirmationService>,
    ) {
        *crate::lock_or_recover(&self.confirmations, "openapi.confirmations") =
            Arc::downgrade(service);
    }
    pub(crate) fn prepare_operation(
        &self,
        operation: &str,
        args: &Value,
    ) -> Result<PreparedOperation, (OpenApiError, usize)> {
        let _gate = crate::lock_or_recover(&self.store.openapi_gate, "openapi.prepare");
        let snapshot = self.snapshot();
        let cfg = snapshot.config;
        if cfg.state_word() != "ready" {
            return Err((
                OpenApiError::new(
                    if cfg.state_word() == "unconfigured" {
                        OPENAPI_UNCONFIGURED
                    } else {
                        OPENAPI_DISABLED
                    },
                    "OpenAPI is unavailable",
                    false,
                ),
                0,
            ));
        }
        let op = cfg.find_operation(operation).cloned().ok_or_else(|| {
            (
                OpenApiError::new(
                    OPENAPI_OPERATION_UNKNOWN,
                    "operation is not whitelisted",
                    false,
                ),
                0,
            )
        })?;
        if !op.method.eq_ignore_ascii_case("get")
            && op.effect_level == crate::config::EffectLevel::Query
        {
            return Err((
                OpenApiError::new(
                    "OPENAPI_INVALID_EFFECT",
                    "non-GET operations require a write effect",
                    false,
                ),
                0,
            ));
        }
        let token = snapshot.token;
        let revision = snapshot.revision;
        if revision.is_empty() {
            return Err((
                OpenApiError::new(
                    "OPENAPI_CONFIG_ERROR",
                    "execution revision unavailable",
                    false,
                ),
                0,
            ));
        }
        let request = self.prepare_http(&cfg, &op, args)?;
        let args = &request.arguments;
        let mut secrets = token.as_ref().map(|t| vec![json!(t)]).unwrap_or_default();
        if let Some(args) = args.as_object() {
            for (name, value) in args {
                if sensitive(name) || op.params.iter().any(|p| p.name == *name && p.sensitive) {
                    secrets.push(value.clone());
                }
            }
        }

        let readback = match &op.verification {
            Some(VerificationStrategy::ReadbackOp { op_id, .. }) => {
                let rb = cfg
                    .find_operation(op_id)
                    .filter(|o| {
                        o.method.eq_ignore_ascii_case("get") && o.effect_level == EffectLevel::Query
                    })
                    .ok_or_else(|| {
                        (
                            OpenApiError::new(
                                OPENAPI_READBACK_FAILED,
                                "readback must be a whitelisted Query GET",
                                false,
                            ),
                            0,
                        )
                    })?;
                Some(self.prepare_http(
                    &cfg,
                    rb,
                    &crate::tool::openapi_verify::build_readback_args(args, rb),
                )?)
            }
            _ => None,
        };
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&json!([
                    revision,
                    request.method.as_str(),
                    request.url,
                    request.body_obj
                ]))
                .unwrap_or_default()
            )
        );
        let preview = if op.effect_level == EffectLevel::Destructive {
            preview(&op, &request, args, token.as_deref())?
        } else {
            Value::Null
        };
        let canonical_args = args.clone();
        Ok(PreparedOperation {
            cfg,
            op,
            request,
            readback,
            token,
            revision,
            digest,
            preview,
            args: canonical_args,
            secrets,
        })
    }
    pub(super) fn prepare_http(
        &self,
        cfg: &OpenApiConfig,
        op: &OpenApiOperation,
        args: &Value,
    ) -> Result<PreparedHttp, (OpenApiError, usize)> {
        if !cfg.has_operation_base() {
            return Err((
                OpenApiError::new(
                    "OPENAPI_BASE_URL",
                    "the operation base cannot contain a query or fragment",
                    false,
                ),
                0,
            ));
        }
        let Some(base) = cfg.effective_base() else {
            return Err((
                OpenApiError::new(OPENAPI_UNCONFIGURED, "没有可用的服务地址", false),
                0,
            ));
        };
        let Some(args) = args.as_object() else {
            return Err((
                OpenApiError::new(OPENAPI_ARGUMENTS, "arguments 必须是对象", false),
                0,
            ));
        };
        if serde_json::to_vec(args)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
            > MAX_ARGS_BYTES
        {
            return Err((
                OpenApiError::new(OPENAPI_ARGUMENTS, "arguments 太大", false),
                0,
            ));
        }
        // Unknown keys are a hard error: a typo must not silently become a
        // request with a different meaning.
        for key in args.keys() {
            if !op.params.iter().any(|p| &p.name == key) {
                return Err((
                    OpenApiError::new(
                        OPENAPI_PARAM_NOT_ALLOWED,
                        format!("参数 '{key}' 不在操作 '{}' 的声明里", op.id),
                        false,
                    ),
                    0,
                ));
            }
        }
        let mut path_values: Vec<(&str, String)> = Vec::new();
        let mut query: Vec<(String, String)> = Vec::new();
        let mut body_obj = serde_json::Map::new();
        let mut canonical_args = serde_json::Map::new();
        for p in &op.params {
            let value = args.get(&p.name).filter(|v| !v.is_null());
            let Some(value) = value else {
                if p.required {
                    return Err((
                        OpenApiError::new(
                            OPENAPI_PARAM_REQUIRED,
                            format!("缺少必填参数 '{}'", p.name),
                            false,
                        ),
                        0,
                    ));
                }
                continue;
            };
            if p.location == OpenApiParamLocation::Body {
                let v = normalize_body_param(p, value)
                    .map_err(|detail| (OpenApiError::new(OPENAPI_PARAM_TYPE, detail, false), 0))?;
                canonical_args.insert(p.name.clone(), v.clone());
                body_obj.insert(p.name.clone(), v);
                continue;
            }
            let normalized = normalize_param(p, value)
                .map_err(|detail| (OpenApiError::new(OPENAPI_PARAM_TYPE, detail, false), 0))?;
            if !p.enum_values.is_empty() && !p.enum_values.contains(&normalized) {
                return Err((
                    OpenApiError::new(
                        OPENAPI_PARAM_ENUM,
                        format!("参数 '{}' 只能是 {}", p.name, p.enum_values.join(" | ")),
                        false,
                    ),
                    0,
                ));
            }
            let canonical = if p.kind == "string" {
                json!(normalized)
            } else {
                serde_json::from_str(&normalized).map_err(|_| {
                    (
                        OpenApiError::new(
                            OPENAPI_PARAM_TYPE,
                            "parameter cannot be represented exactly",
                            false,
                        ),
                        0,
                    )
                })?
            };
            canonical_args.insert(p.name.clone(), canonical);
            match p.location {
                OpenApiParamLocation::Path => {
                    // Audit P1: `.`/`..` (or a slash) in a path parameter can
                    // escape the whitelisted template after URL
                    // normalisation (`/rooms/../temperature` → `/temperature`)
                    // and would carry the token to an unintended resource.
                    if normalized == "."
                        || normalized == ".."
                        || normalized.contains('/')
                        || normalized.contains('\\')
                    {
                        return Err((
                            OpenApiError::new(
                                OPENAPI_PARAM_NOT_ALLOWED,
                                format!("参数 '{}' 不能是 . / .. 或包含路径分隔符", p.name),
                                false,
                            ),
                            0,
                        ));
                    }
                    path_values.push((p.name.as_str(), normalized));
                }
                OpenApiParamLocation::Query => query.push((p.name.clone(), normalized)),
                OpenApiParamLocation::Body => unreachable!(),
            }
        }
        let mut path = op.path.clone();
        for (name, value) in path_values {
            path = path.replace(&format!("{{{name}}}"), &pct(&value));
        }
        // The template and every parameter were checked, but check the FINAL
        // path too: URL normalisation is the last place a dot segment could
        // escape the whitelist (audit round 2).
        if let Err(detail) = safe_final_path(&path) {
            return Err((
                OpenApiError::new(OPENAPI_PARAM_NOT_ALLOWED, detail, false),
                0,
            ));
        }
        let mut url = format!("{}{}", base.trim_end_matches('/'), path);
        if !query.is_empty() {
            let qs = query
                .iter()
                .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
                .collect::<Vec<_>>()
                .join("&");
            if qs.chars().count() > MAX_QUERY_CHARS {
                return Err((
                    OpenApiError::new(OPENAPI_ARGUMENTS, "查询参数太长", false),
                    0,
                ));
            }
            url.push('?');
            url.push_str(&qs);
        }
        let method = match op.method.to_ascii_lowercase().as_str() {
            "get" => reqwest::Method::GET,
            "post" => reqwest::Method::POST,
            "put" => reqwest::Method::PUT,
            "patch" => reqwest::Method::PATCH,
            "delete" => reqwest::Method::DELETE,
            other => {
                return Err((
                    OpenApiError::new(
                        OPENAPI_METHOD_UNSUPPORTED,
                        format!("不支持的 HTTP 方法 '{other}'"),
                        false,
                    ),
                    0,
                ));
            }
        };
        Ok(PreparedHttp {
            method,
            url,
            body_obj,
            arguments: Value::Object(canonical_args),
            retry_post: args.keys().any(|key| {
                key.eq_ignore_ascii_case("idempotency_key")
                    || key.eq_ignore_ascii_case("idempotency-key")
            }),
        })
    }
}
fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "credential",
        "api_key",
        "apikey",
    ]
    .iter()
    .any(|s| name.contains(s))
}
fn preview(
    op: &OpenApiOperation,
    request: &PreparedHttp,
    args: &Value,
    token: Option<&str>,
) -> Result<Value, (OpenApiError, usize)> {
    let fail = || {
        (
            OpenApiError::new(
                "CONFIRMATION_PREVIEW_UNSAFE",
                "operation target cannot be safely previewed",
                false,
            ),
            0,
        )
    };
    let mut target = reqwest::Url::parse(&request.url).map_err(|_| fail())?;
    if !target.username().is_empty() || target.password().is_some() {
        return Err(fail());
    }
    if op
        .params
        .iter()
        .any(|p| p.location == OpenApiParamLocation::Path && (p.sensitive || sensitive(&p.name)))
    {
        return Err(fail());
    }
    target.set_query(None);
    target.set_fragment(None);
    if token.filter(|t| !t.is_empty()).is_some_and(|t| {
        target.as_str().contains(t)
            || target.as_str().contains(&pct(t))
            || decoded_target(target.as_str()).contains(t)
            || op.id.contains(t)
    }) {
        return Err(fail());
    }
    let mut parameters = args.clone();
    if let Some(obj) = parameters.as_object_mut() {
        for (name, value) in obj {
            if sensitive(name) || op.params.iter().any(|p| p.name == *name && p.sensitive) {
                *value = json!("[redacted]");
            } else if let Some(t) = token.filter(|t| !t.is_empty()) {
                redact_secret(value, t);
                redact_secret(value, &pct(t));
            }
        }
    }
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        redact_secret(&mut parameters, token);
        redact_secret(&mut parameters, &pct(token));
    }
    if serde_json::to_vec(&parameters).map_or(true, |p| p.len() > MAX_ARGS_BYTES) {
        return Err(fail());
    }
    Ok(
        json!({"tool":"openapi.call","operation":op.id,"service":target.host_str(),"method":op.method.to_ascii_uppercase(),"target":target.as_str(),"parameters":parameters}),
    )
}

fn decoded_target(target: &str) -> String {
    let bytes = target.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn scrub_result(result: &mut ToolResult, secrets: &[Value]) {
    if let Some(payload) = &mut result.result {
        scrub_value(payload, secrets);
    }
    if let Some(error) = &mut result.error {
        scrub_text(&mut error.detail, secrets);
    }
    if let Some(fact) = &mut result.fact {
        scrub_text(&mut fact.subject, secrets);
        if let Some(value) = &mut fact.value {
            scrub_text(value, secrets);
        }
        if let Some(detail) = &mut fact.detail {
            scrub_text(detail, secrets);
        }
    }
}
pub(crate) fn scrub_value(value: &mut Value, secrets: &[Value]) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if sensitive(key) {
                    *value = json!("[redacted]");
                } else {
                    scrub_value(value, secrets);
                }
            }
            for secret in secrets.iter().filter_map(Value::as_str) {
                redact_secret(value, secret);
                redact_secret(value, &pct(secret));
            }
        }
        Value::Array(array) => {
            for item in array {
                scrub_value(item, secrets);
            }
        }
        Value::String(text) => scrub_text(text, secrets),
        other => {
            if secrets.iter().any(|secret| secret == other) {
                *other = json!("[redacted]");
            }
        }
    }
}
fn scrub_text(text: &mut String, secrets: &[Value]) {
    for secret in secrets {
        let raw = secret
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| secret.to_string());
        if raw.is_empty() {
            continue;
        }
        let escaped = serde_json::to_string(&raw).unwrap_or_default();
        for value in [
            raw.clone(),
            pct(&raw),
            escaped.trim_matches('"').to_string(),
        ] {
            if !value.is_empty() {
                *text = text.replace(&value, "[redacted]");
            }
        }
    }
}
