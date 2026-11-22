//! Immutable model-facing directory shared by all registration domains.

use super::Tool;
use crate::llm::ToolSpec;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(
        "invalid canonical tool name '{0}': expected 1..=64 ASCII letters, digits, '.', '_' or '-'"
    )]
    InvalidName(String),
    #[error("duplicate canonical tool name '{0}'")]
    Duplicate(String),
    #[error("tool wire ID collision '{0}'")]
    WireCollision(String),
}

pub(super) fn validate_name(name: &str) -> Result<(), CatalogError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    {
        return Err(CatalogError::InvalidName(name.to_string()));
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Wire IDs are stable, provider-safe, human-recognizable to the LLM, and
/// collision-resistant across underscore/dot/hyphen syntax (always 63 chars).
fn wire_id(canonical: &str) -> String {
    let readable: String = canonical
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(44)
        .collect();
    let prefix = format!("hm_{readable}_");
    let hash = digest(canonical.as_bytes());
    format!("{prefix}{}", &hash[..(63 - prefix.len())])
}

struct Entry {
    tool: Arc<dyn Tool>,
    revision: String,
    spec: ToolSpec,
}

pub struct CatalogSnapshot {
    entries: BTreeMap<String, Entry>,
    inverse: BTreeMap<String, String>,
    pub revision: String,
}

impl CatalogSnapshot {
    pub(super) fn build(tools: Vec<Arc<dyn Tool>>) -> Result<Self, CatalogError> {
        Self::build_with_ids(tools, wire_id)
    }

    fn build_with_ids(
        tools: Vec<Arc<dyn Tool>>,
        make_wire: impl Fn(&str) -> String,
    ) -> Result<Self, CatalogError> {
        let mut entries = BTreeMap::new();
        let mut inverse = BTreeMap::new();
        for tool in tools {
            let descriptor = tool.descriptor();
            let canonical = descriptor.name.to_string();
            validate_name(&canonical)?;
            if entries.contains_key(&canonical) {
                return Err(CatalogError::Duplicate(canonical));
            }
            let wire = make_wire(&canonical);
            if inverse.insert(wire.clone(), canonical.clone()).is_some() {
                return Err(CatalogError::WireCollision(wire));
            }
            let availability = tool.availability();
            let identity = tool
                .catalog_revision()
                .unwrap_or_else(|| format!("{:p}", Arc::as_ptr(&tool)));
            let revision = digest(
                serde_json::json!({
                    "descriptor": descriptor, "availability": availability, "identity": identity,
                })
                .to_string()
                .as_bytes(),
            );
            let spec = ToolSpec {
                name: wire,
                canonical_name: canonical.clone(),
                description: format!(
                    "{canonical}: {} (side effect: {:?})",
                    descriptor.description, descriptor.side_effect
                ),
                parameters: descriptor.parameters,
            };
            entries.insert(
                canonical,
                Entry {
                    tool,
                    revision,
                    spec,
                },
            );
        }
        let revision = digest(
            entries
                .iter()
                .map(|(name, e)| format!("{name}:{};", e.revision))
                .collect::<String>()
                .as_bytes(),
        );
        Ok(Self {
            entries,
            inverse,
            revision,
        })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        // Preserve the pre-catalog inclusion policy. Availability controls
        // capability display and tool execution, not name advertisement.
        self.entries.values().map(|e| e.spec.clone()).collect()
    }

    pub fn contains(&self, canonical: &str) -> bool {
        self.entries.contains_key(canonical)
    }

    pub fn canonical_name(&self, wire: &str) -> Option<&str> {
        self.inverse.get(wire).map(String::as_str)
    }

    pub(super) fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.entries.values().map(|e| e.tool.clone()).collect()
    }

    pub(crate) fn tool(&self, canonical: &str) -> Option<Arc<dyn Tool>> {
        self.entries.get(canonical).map(|e| e.tool.clone())
    }

    pub(super) fn same_entry(&self, other: &Self, canonical: &str) -> bool {
        self.entries
            .get(canonical)
            .zip(other.entries.get(canonical))
            .is_some_and(|(a, b)| a.revision == b.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{SideEffect, ToolCtx, ToolDescriptor, ToolResult};

    struct Probe(&'static str);
    #[async_trait::async_trait]
    impl Tool for Probe {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.0.into(),
                service: "test".into(),
                side_effect: SideEffect::Query,
                description: "fixture".into(),
                parameters: serde_json::json!({"type":"object"}),
            }
        }
        async fn execute(&self, _: serde_json::Value, _: &ToolCtx) -> ToolResult {
            unreachable!()
        }
    }

    #[test]
    fn even_a_wire_digest_collision_is_rejected_without_publication() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Probe("family_light.start")),
            Arc::new(Probe("family.light_start")),
        ];
        let result = CatalogSnapshot::build_with_ids(tools, |_| "forced_collision".into());
        assert!(matches!(result, Err(CatalogError::WireCollision(_))));
    }
}
