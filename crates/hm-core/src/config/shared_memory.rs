//! Offline-resilient shared memory configuration (`#80`).

use serde::{Deserialize, Serialize};

pub const SHARED_MEMORY_KEY: &str = "shared_memory";
pub const SHARED_MEMORY_TOKEN_SECRET: &str = "shared_memory_token";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SharedMemoryConfig {
    pub enabled: bool,
    pub nas_base_url: String,
    pub upload_enabled: bool,
}

impl SharedMemoryConfig {
    pub fn normalized(mut self) -> Self {
        self.nas_base_url = self.nas_base_url.trim().trim_end_matches('/').to_string();
        self
    }

    pub fn validate(&self) -> Result<(), String> {
        let url = self.nas_base_url.trim();
        if url.is_empty() {
            return Ok(());
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("nas_base_url must start with http:// or https://".into());
        }
        Ok(())
    }
}
