//! Commit a local service edit and its sealed credential as one runtime generation.
use super::ConfigDelivery;
use crate::config::{
    decode_services, ConfigError, ServiceConfig, ServiceStatus, ServicesConfig, CONFIG_KEY,
};
use crate::store::Store;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::json;

impl ConfigDelivery {
    pub(crate) fn save_local_service(
        store: &Store,
        service: &str,
        cfg: ServiceConfig,
        api_key: Option<&str>,
        actor: &str,
    ) -> Result<ServiceStatus, ConfigError> {
        let slot: fn(&mut ServicesConfig) -> &mut ServiceConfig = match service {
            "llm" => |all| &mut all.llm,
            "llm_fallback" => |all| &mut all.llm_fallback,
            "asr" => |all| &mut all.asr,
            "tts" => |all| &mut all.tts,
            _ => return Err(ConfigError::UnknownService(service.into())),
        };
        let name = format!("{service}.api_key");
        let sealed = api_key
            .filter(|key| !key.is_empty())
            .map(|key| store.seal_secret(&name, key))
            .transpose()?;
        let key_set = store.with(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let before: Option<String> = tx
                .query_row("SELECT value FROM config_kv WHERE key=?1", [CONFIG_KEY], |r| r.get(0))
                .optional()?;
            let mut all = decode_services(before.as_deref())?;
            *slot(&mut all) = cfg.clone();
            tx.execute(
                "INSERT INTO config_kv(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![CONFIG_KEY, serde_json::to_string(&all)?],
            )?;
            if let Some(value) = &sealed {
                tx.execute(
                    "INSERT INTO secrets(name,value) VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                    params![name, value],
                )?;
            } else if api_key.is_some() {
                tx.execute("DELETE FROM secrets WHERE name=?1", [&name])?;
            }
            let key_set = store
                .secret_get_on(&tx, &name)?
                .or_else(|| std::env::var(&cfg.api_key_env).ok())
                .is_some_and(|key| !key.trim().is_empty());
            let payload = json!({
                "provider": cfg.provider, "base_url": cfg.base_url, "model": cfg.model,
                "api_key_env": cfg.api_key_env, "voice": cfg.voice, "key_set": key_set,
                "thinking": cfg.thinking,
            });
            tx.execute(
                "INSERT INTO config_history(ts_ms,actor,scope,summary,payload) VALUES (?1,?2,?3,?4,?5)",
                params![crate::id::now_ms() as i64, actor, service,
                    format!("provider={} model={}", cfg.provider, cfg.model), payload.to_string()],
            )?;
            tx.commit()?;
            Ok(key_set)
        })?;
        Ok(ServiceStatus {
            provider: cfg.provider,
            base_url: cfg.base_url,
            model: cfg.model,
            api_key_env: cfg.api_key_env,
            api_key_set: key_set,
            voice: cfg.voice,
            thinking: cfg.thinking,
        })
    }
}
