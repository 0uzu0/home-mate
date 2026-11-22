//! Immutable per-operation service configuration and credential bindings.
use super::{decode_services, ConfigService, ServicesConfig, CONFIG_KEY};
use crate::{
    llm::{deepseek::DeepSeekClient, LlmClient},
    speech,
};
use rusqlite::OptionalExtension;
use std::{collections::BTreeMap, sync::Arc};

pub struct RuntimeServices {
    pub config: ServicesConfig,
    keys: BTreeMap<&'static str, Result<Option<String>, String>>,
}

impl ConfigService {
    /// Pin published settings and their credential bindings in one SQLite
    /// read transaction. The resulting snapshot outlives the transaction.
    pub fn runtime_snapshot(&self) -> Result<RuntimeServices, String> {
        self.store
            .with(|c| {
                let tx = c.unchecked_transaction()?;
                let raw: Option<String> = tx
                    .query_row(
                        "SELECT value FROM config_kv WHERE key=?1",
                        [CONFIG_KEY],
                        |r| r.get(0),
                    )
                    .optional()?;
                let config = decode_services(raw.as_deref())?;
                let mut keys = BTreeMap::new();
                for (name, slot) in [
                    ("llm", &config.llm),
                    ("llm_fallback", &config.llm_fallback),
                    ("asr", &config.asr),
                    ("tts", &config.tts),
                ] {
                    let key = self
                        .store
                        .secret_get_on(&tx, &format!("{name}.api_key"))
                        .map_err(|e| e.to_string())
                        .map(|stored| stored.or_else(|| std::env::var(&slot.api_key_env).ok()));
                    keys.insert(name, key);
                }
                tx.commit()?;
                Ok(RuntimeServices { config, keys })
            })
            .map_err(|e| e.to_string())
    }
}

impl RuntimeServices {
    fn key(&self, name: &str) -> Result<Option<&str>, String> {
        self.keys
            .get(name)
            .ok_or_else(|| "unknown service slot".to_string())?
            .as_ref()
            .map(|key| key.as_deref())
            .map_err(Clone::clone)
    }
    pub fn llm(&self, name: &str) -> Result<DeepSeekClient, String> {
        let slot = match name {
            "llm" => &self.config.llm,
            "llm_fallback" => &self.config.llm_fallback,
            _ => return Err("unknown LLM slot".into()),
        };
        if !slot.is_configured() || slot.base_url.is_empty() {
            return Err("LLM 未配置：请在「服务」页设置服务商与接口地址".into());
        }
        DeepSeekClient::with_thinking(
            &slot.base_url,
            self.key(name)?.unwrap_or_default(),
            &slot.thinking,
        )
        .map_err(|e| e.to_string())
    }
    pub fn llm_client(&self, name: &str) -> Result<Arc<dyn LlmClient>, String> {
        self.llm(name)
            .map(|client| Arc::new(client) as Arc<dyn LlmClient>)
    }
    pub fn tts(&self) -> Result<Arc<dyn speech::TtsClient>, String> {
        if !self.config.tts.is_configured() {
            return Err("TTS 未配置：语音播报关闭，文本路径不受影响".into());
        }
        speech::tts_from_config(&self.config.tts, self.key("tts")?).map_err(|e| e.to_string())
    }
    pub fn asr(&self) -> Result<Arc<dyn speech::AsrClient>, String> {
        if self.config.asr.provider.is_empty() || self.config.asr.provider == "mock" {
            return Err("ASR 未配置（或为 mock）：语音唤醒不可用，文本路径不受影响".into());
        }
        speech::asr_from_config(&self.config.asr, self.key("asr")?).map_err(|e| e.to_string())
    }
}
