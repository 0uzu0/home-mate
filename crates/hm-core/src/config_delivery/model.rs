use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub type Fields = BTreeMap<String, Value>;
pub type Result<T> = std::result::Result<T, DeliveryError>;

#[derive(Debug, Serialize)]
pub struct DeliveryError {
    pub code: &'static str,
    pub detail: String,
    pub pointer: Option<String>,
    #[serde(skip)]
    pub status: u16,
}
impl DeliveryError {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            pointer: None,
            status: 400,
        }
    }
    pub fn conflict(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: 409,
            ..Self::new(code, detail)
        }
    }
    pub fn at(mut self, path: &str) -> Self {
        self.pointer = Some(format!("/{}", path.replace('.', "/")));
        self
    }
    pub fn internal(detail: impl std::fmt::Display) -> Self {
        Self {
            status: 500,
            ..Self::new("CONFIG_STORAGE_ERROR", detail.to_string())
        }
    }
}
impl From<rusqlite::Error> for DeliveryError {
    fn from(e: rusqlite::Error) -> Self {
        Self::internal(e)
    }
}
impl From<serde_json::Error> for DeliveryError {
    fn from(e: serde_json::Error) -> Self {
        Self::new("INVALID_DOCUMENT", e.to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    FamilyPolicy,
    NodeConfig,
    SameNodeRestore,
}
impl Purpose {
    pub fn layer(&self) -> &'static str {
        if *self == Self::FamilyPolicy {
            "family"
        } else {
            "node"
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub node_id: String,
    pub config_revision: u64,
    pub core_version: String,
    pub schema_version: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub household_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub core_api_schema: u32,
    pub gateway_protocol_min: u32,
    pub gateway_protocol_max: u32,
    pub required_capabilities: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Set { path: String, value: Value },
    Replace { path: String, value: Value },
    Clear { path: String },
    Preserve { path: String },
}
impl Operation {
    pub fn path(&self) -> &str {
        match self {
            Self::Set { path, .. }
            | Self::Replace { path, .. }
            | Self::Clear { path }
            | Self::Preserve { path } => path,
        }
    }
    pub fn action(&self) -> &'static str {
        match self {
            Self::Set { .. } => "set",
            Self::Replace { .. } => "replace",
            Self::Clear { .. } => "clear",
            Self::Preserve { .. } => "preserve",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub name: String,
    pub schema_version: u32,
    pub operations: Vec<Operation>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Binding {
    Preserve { binding_id: String },
    RequireBinding { binding_id: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub format: String,
    pub version: u32,
    pub package_id: String,
    pub package_revision: u64,
    pub created_ms: u64,
    pub purpose: Purpose,
    pub source: Source,
    pub target: Target,
    pub compatibility: Compatibility,
    pub change_reason: String,
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub bindings: Vec<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}
impl Package {
    pub fn hash(&self) -> Result<String> {
        let mut value = serde_json::to_value(self)?;
        value.as_object_mut().unwrap().remove("content_hash");
        digest(&value)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Layers {
    pub family: Fields,
    pub node: Fields,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diff {
    pub path: String,
    pub before: Value,
    pub after: Value,
    pub origin: String,
    pub action: String,
    pub layer_before: Option<Value>,
    pub layer_after: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub layer_before_null: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub layer_after_null: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commit {
    pub plan_id: String,
    pub plan_hash: String,
    pub expected_revision: u64,
    pub application_id: String,
}
pub fn digest(value: &Value) -> Result<String> {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let ordered: BTreeMap<_, _> =
                    m.iter().map(|(k, v)| (k.clone(), sorted(v))).collect();
                serde_json::to_value(ordered).unwrap()
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            _ => v.clone(),
        }
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&sorted(value))?)
    ))
}
pub fn uuid() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(DeliveryError::internal)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let s: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &s[..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..]
    ))
}
pub fn valid_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
