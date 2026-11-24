use super::{discovery::Block as DiscoveryBlock, inventory, *};
use crate::config::{ServiceConfig, ServicesConfig, SharedMemoryConfig};
use crate::store::Store;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const BLOCKS: &[&str] = &[
    "services",
    "shared_memory",
    "persona",
    "weather",
    "home",
    "music",
    "news",
    "search",
    "openapi",
    "mode",
    "open_sockets",
    "reflex",
    "node",
    "audio_eq",
    "device_led_configs",
    "unit_acoustic_profiles",
];
pub(super) const PUBLISHED_BLOCKS: &[&str] = &[
    "persona",
    "services",
    "weather",
    "home",
    "music",
    "news",
    "search",
    "openapi",
    "mode",
    "open_sockets",
    "reflex",
    "node",
    "audio_eq",
    "device_led_configs",
    "unit_acoustic_profiles",
];

pub struct ConfigDelivery {
    pub(super) store: Arc<Store>,
    voice: Option<Arc<crate::voice::VoicePipeline>>,
    open_sockets: Option<super::open_sockets::Provider>,
}
struct Snapshot {
    revision: u64,
    epoch: u64,
    household: String,
    node: String,
    stored: Fields,
    layers: Layers,
    origins: BTreeMap<String, String>,
    baseline: String,
    adopted: Vec<Diff>,
}
pub(super) fn raw(c: &Connection, key: &str) -> Result<Option<String>> {
    Ok(
        c.query_row("SELECT value FROM config_kv WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?,
    )
}
fn binding_available(store: &Store, c: &Connection, name: &str) -> Result<bool> {
    if matches!(
        name,
        crate::config::MUSIC_LIBRARY_SECRET
            | crate::config::MUSIC_ONLINE_SECRET
            | crate::config::OPENAPI_TOKEN_SECRET
    ) {
        return store
            .secret_get_on(c, name)
            .map(|value| value.is_some_and(|value| !value.is_empty()))
            .map_err(DeliveryError::internal);
    }
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM secrets WHERE name=?1 AND value!='')",
        [name],
        |r| r.get(0),
    )?)
}

fn defaults() -> Fields {
    let mut fields = Fields::new();
    let services = ServicesConfig {
        llm: ServiceConfig {
            provider: "deepseek".into(),
            base_url: crate::config::default_llm_base_url(),
            model: "deepseek-flash".into(),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            thinking: "disabled".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    flatten(
        "services",
        &serde_json::to_value(services).unwrap(),
        &mut fields,
    );
    flatten(
        "shared_memory",
        &serde_json::to_value(SharedMemoryConfig::default()).unwrap(),
        &mut fields,
    );
    fields.insert("persona.agent_md.content".into(), json!(""));
    flatten(
        "weather",
        &json!(crate::config::WeatherConfig::default()),
        &mut fields,
    );
    flatten(
        "mode",
        &super::mode::projection(&crate::voice::VoiceSettings::default()),
        &mut fields,
    );
    flatten(
        "audio_eq",
        &json!(crate::config::AudioEqConfig::default()),
        &mut fields,
    );
    flatten(
        "reflex",
        &json!(crate::config::ReflexConfig::default()),
        &mut fields,
    );
    fields.insert("node.wake.phrases".into(), json!([]));
    fields.insert("node.led.timezone".into(), json!("Asia/Shanghai"));
    for block in DiscoveryBlock::ALL {
        flatten(block.key(), &block.defaults(), &mut fields);
    }
    fields
}
pub(super) fn flatten(prefix: &str, value: &Value, out: &mut Fields) {
    // Optional connections are layered as a whole value, including null.
    if matches!(prefix, "music.library" | "music.online") {
        out.insert(prefix.into(), value.clone());
        return;
    }
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            flatten(&format!("{prefix}.{key}"), value, out);
        }
    } else {
        out.insert(prefix.into(), value.clone());
    }
}
fn resolved(layers: &Layers) -> (Fields, BTreeMap<String, String>) {
    let mut result = defaults();
    let mut origins = result
        .keys()
        .map(|k| (k.clone(), "installed_default".into()))
        .collect::<BTreeMap<_, _>>();
    for (layer, values) in [("family", &layers.family), ("node", &layers.node)] {
        for (path, value) in values {
            result.insert(path.clone(), value.clone());
            origins.insert(path.clone(), layer.into());
        }
    }
    (result, origins)
}
fn snapshot(c: &Connection, store: &Store) -> Result<Snapshot> {
    let (revision, epoch, household, node, layers, last): (u64,u64,String,String,String,String) = c.query_row(
        "SELECT revision,data_epoch,household_id,node_id,layers_json,resolved_json FROM config_head WHERE id=1", [],
        |r| Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    let mut stored = defaults();
    for key in ["services", "shared_memory"] {
        if let Some(text) = raw(c, key)? {
            // Persisted partial legacy structs use their serde defaults, which
            // differ from a fresh install's default LLM connection.
            let value = if key == "services" {
                serde_json::to_value(serde_json::from_str::<ServicesConfig>(&text)?)?
            } else {
                serde_json::to_value(serde_json::from_str::<SharedMemoryConfig>(&text)?)?
            };
            let mut flattened = Fields::new();
            flatten(key, &value, &mut flattened);
            for (path, value) in flattened {
                if stored.contains_key(&path) {
                    stored.insert(path, value);
                }
            }
        }
    }
    let weather = super::weather::published(c)?;
    if let Some(cfg) = &weather {
        flatten("weather", &serde_json::to_value(cfg)?, &mut stored);
    }
    let mut absent = Vec::new();
    if weather.is_none() {
        absent.push("weather");
    }
    for block in DiscoveryBlock::ALL {
        if let Some(value) = block.published(c, store)? {
            flatten(block.key(), &value, &mut stored);
        } else {
            absent.push(block.key());
        }
    }
    if let Some(voice) = super::mode::published(c)? {
        flatten("mode", &voice, &mut stored);
    } else {
        absent.push("mode");
    }
    if let Some(text) = raw(c, crate::config::AUDIO_EQ_KEY)? {
        if let Ok(eq) = serde_json::from_str::<crate::config::AudioEqConfig>(&text) {
            flatten("audio_eq", &serde_json::to_value(eq)?, &mut stored);
        }
    }
    if let Some(text) = raw(c, crate::config::REFLEX_KEY)? {
        if let Ok(rfx) = serde_json::from_str::<crate::config::ReflexConfig>(&text) {
            flatten("reflex", &serde_json::to_value(rfx)?, &mut stored);
        }
    }
    for (kv_key, root) in [
        ("node", "node"),
        (crate::config::DEVICE_LED_CONFIGS_KEY, "device_led_configs"),
        (
            crate::config::UNIT_ACOUSTIC_PROFILES_KEY,
            "unit_acoustic_profiles",
        ),
    ] {
        if let Some(text) = raw(c, kv_key)? {
            if let Ok(val) = serde_json::from_str::<Value>(&text) {
                flatten(root, &val, &mut stored);
            }
        }
    }
    let is_absent = |path: &str| absent.contains(&path.split('.').next().unwrap_or(""));
    let content: String = c
        .query_row(
            "SELECT content FROM agent_md_revisions ORDER BY version DESC,id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();
    stored.insert("persona.agent_md.content".into(), json!(content));
    let mut layers: Layers = if layers == "{}" {
        Layers {
            node: stored
                .iter()
                .filter(|(path, _)| !is_absent(path))
                .map(|(path, value)| (path.clone(), value.clone()))
                .collect(),
            ..Default::default()
        }
    } else {
        serde_json::from_str(&layers)?
    };
    let last: Fields = serde_json::from_str(&last)?;
    let mut adopted = vec![];
    let mut pending_projection = Fields::new();
    if let Some(value) = super::worker::expected_projection(c, epoch, "persona")? {
        pending_projection.insert(super::worker::PERSONA.into(), value);
    }
    if let Some(value) = super::worker::expected_projection(c, epoch, "services")? {
        flatten("services", &value, &mut pending_projection);
    }
    if let Some(value) = super::worker::expected_projection(c, epoch, "weather")? {
        flatten("weather", &value, &mut pending_projection);
    }
    if let Some(value) = super::worker::expected_projection(c, epoch, "mode")? {
        flatten("mode", &value, &mut pending_projection);
    }
    for block in DiscoveryBlock::ALL {
        if let Some(value) = super::worker::expected_projection(c, epoch, block.key())? {
            flatten(block.key(), &value, &mut pending_projection);
        }
    }
    for key in [
        "reflex",
        "node",
        "audio_eq",
        "device_led_configs",
        "unit_acoustic_profiles",
    ] {
        if let Some(value) = super::worker::expected_projection(c, epoch, key)? {
            flatten(key, &value, &mut pending_projection);
        }
    }
    for (path, value) in &stored {
        if pending_projection.get(path) == Some(value) {
            continue;
        }
        if is_absent(path) && !last.contains_key(path) {
            continue;
        }
        if !last.is_empty() && last.get(path) != Some(value) {
            layers.node.insert(path.clone(), value.clone());
            adopted.push(Diff {
                path: path.clone(),
                before: last.get(path).cloned().unwrap_or(Value::Null),
                after: value.clone(),
                origin: "legacy_node_override".into(),
                action: "adopt_external_write".into(),
                layer_before: last.get(path).cloned(),
                layer_after: Some(value.clone()),
                layer_before_null: last.get(path).is_some_and(Value::is_null),
                layer_after_null: value.is_null(),
            });
        }
    }
    // Authentication validity is checked on each request. Its last-used time
    // is not a configuration change and must not invalidate a ten-minute plan.
    let mut stmt =
        c.prepare("SELECT key,value FROM config_kv WHERE key NOT IN ('api_tokens','db.maintenance') ORDER BY key")?;
    let all = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
    let schema: i64 = c.query_row(
        "SELECT COALESCE(MAX(version),0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    let baseline = digest(
        &json!({"revision":revision,"epoch":epoch,"schema":schema,"core_build":crate::BUILD_ID,"all_config":all,"persona":content,"household":household,"node":node,"layers":layers}),
    )?;
    let (_, origins) = resolved(&layers);
    Ok(Snapshot {
        revision,
        epoch,
        household,
        node,
        stored,
        layers,
        origins,
        baseline,
        adopted,
    })
}
fn begin(c: &Connection) -> Result<Transaction<'_>> {
    Ok(Transaction::new_unchecked(
        c,
        rusqlite::TransactionBehavior::Immediate,
    )?)
}

impl ConfigDelivery {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            voice: None,
            open_sockets: None,
        }
    }

    pub fn for_app(state: &crate::api::AppState) -> Self {
        Self {
            store: state.store.clone(),
            voice: Some(state.voice.clone()),
            open_sockets: Some(super::open_sockets::Provider::from_app(state)),
        }
    }

    fn db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.store
            .with(|c| Ok(f(c)))
            .map_err(DeliveryError::internal)?
    }
    pub fn enroll(&self, node: &str) -> Result<Value> {
        if !crate::fleet::valid_peer_id(node) {
            return Err(DeliveryError::new(
                "NODE_ID_REQUIRED",
                "configure this node's stable identity first",
            ));
        }
        self.db(|c| {
            let tx = begin(c)?;
            let current = snapshot(&tx, &self.store)?;
            if !current.household.is_empty() {
                if current.node != node { return Err(DeliveryError::conflict("TARGET_IDENTITY_CHANGED", "enrolled identity is immutable")); }
            } else {
                tx.execute("UPDATE config_head SET household_id=?1,node_id=?2,revision=revision+1 WHERE id=1", params![uuid()?,node])?;
            }
            let (household,node):(String,String)=tx.query_row("SELECT household_id,node_id FROM config_head WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
            tx.commit()?;
            Ok(json!({"household_id":household,"node_id":node,"enrolled":true,"shared_household_pairing":"not_implemented"}))
        })
    }
    pub fn state(&self, principal: &str) -> Result<Value> {
        let sockets_available = self
            .open_sockets
            .as_ref()
            .and_then(|provider| provider.effective(&self.store, None))
            .is_some();
        self.db(|c| {
            let tx = begin(c)?;
            let s = snapshot(&tx, &self.store)?;
            let bindings = inventory::BINDINGS.iter().map(|name| {
                let bound = binding_available(&self.store, &tx, name)?;
                Ok(json!({"binding_id":name,"bound":bound,"scope":"stored_target_binding_only","environment_binding":"not_probed"}))
            }).collect::<Result<Vec<_>>>()?;
            let schema: i64 = tx.query_row("SELECT COALESCE(MAX(version),0) FROM schema_version", [], |r| r.get(0))?;
            let (effective, _) = resolved(&s.layers);
            let applications = list(&tx, principal)?;
            let mut unavailable_blocks = DiscoveryBlock::ALL.into_iter()
                .filter(|block| block.require_readable(&tx, &self.store).is_err())
                .map(DiscoveryBlock::key).collect::<Vec<_>>();
            if self.voice.is_none() || super::mode::require_readable(&tx).is_err() {
                unavailable_blocks.push("mode");
            }
            let socket_identity_matches = s.node.is_empty() || super::open_sockets::live_node_matches(&tx, &s.node)?;
            if (!sockets_available || !socket_identity_matches) && !unavailable_blocks.contains(&"open_sockets") {
                unavailable_blocks.push("open_sockets");
            }
            Ok(json!({"config_revision":s.revision,"data_epoch":s.epoch,"core_version":crate::CORE_VERSION,"schema_version":schema,
                "target":{"household_id":s.household,"node_id":s.node,"enrolled":!s.household.is_empty()},
                "layer_mode":"partial","fields":inventory::fields(),"stored":s.stored,"resolved":effective,"origins":s.origins,"layers":s.layers,
                "bindings":bindings,"effective_state":"saved_pending",
                "capabilities":{"supported_blocks":BLOCKS,"publication_blocks":PUBLISHED_BLOCKS,"rollback":true,"fleet":true,"enrollment":true,
                    "weather_reviewable":super::weather::require_readable_baseline(&tx).is_ok(),
                    "unavailable_blocks":unavailable_blocks},
                "applications":applications}))
        })
    }
    pub fn preflight(
        &self,
        principal: &str,
        package: Package,
        migration_report: Value,
    ) -> Result<Value> {
        self.preflight_at(principal, package, migration_report, crate::id::now_ms())
    }
    pub(crate) fn preflight_at(
        &self,
        principal: &str,
        package: Package,
        migration_report: Value,
        now: u64,
    ) -> Result<Value> {
        self.preflight_at_revision(principal, package, migration_report, now, None)
    }

    pub(super) fn preflight_at_revision(
        &self,
        principal: &str,
        package: Package,
        migration_report: Value,
        now: u64,
        converted_at_revision: Option<u64>,
    ) -> Result<Value> {
        let competing_sockets = if package
            .blocks
            .iter()
            .any(|block| block.name == "open_sockets")
        {
            Some(
                self.open_sockets
                    .as_ref()
                    .ok_or_else(|| {
                        DeliveryError::conflict(
                            "OPEN_SOCKETS_RUNTIME_UNAVAILABLE",
                            "reviewed sockets require the actual Registry",
                        )
                        .at("open_sockets")
                    })?
                    .competing_names()?,
            )
        } else {
            None
        };
        self.db(|c| {
            let tx = begin(c)?;
            let s = snapshot(&tx, &self.store)?;
            if converted_at_revision.is_some_and(|revision| revision != s.revision) {
                return Err(DeliveryError::conflict(
                    "PLAN_STALE", "configuration changed during legacy conversion",
                ));
            }
            validate_envelope(&package, &s)?;
            if competing_sockets.is_some() {
                super::open_sockets::require_enrolled_identity(&tx, &s.node)?;
            }
            let mut layers = s.layers.clone();
            let mut seen = BTreeSet::new();
            let mut requested = Vec::new();
            for block in &package.blocks {
                if !BLOCKS.contains(&block.name.as_str()) || block.schema_version != 1
                    || ((matches!(block.name.as_str(), "weather" | "mode") || DiscoveryBlock::named(&block.name).is_some()) && package.compatibility.core_api_schema < 2) {
                    return Err(DeliveryError::new("UNSUPPORTED_BLOCK", "block/schema is not supported by this slice").at(&block.name));
                }
                if block.name == "weather" {
                    super::weather::require_readable_baseline(&tx)?;
                }
                if block.name == "mode" {
                    if self.voice.is_none() {
                        return Err(DeliveryError::conflict("VOICE_RUNTIME_UNAVAILABLE", "voice policy requires the active Core runtime").at("mode"));
                    }
                    super::mode::require_readable(&tx)?;
                }
                let discovery = DiscoveryBlock::named(&block.name);
                if let Some(block) = discovery {
                    block.require_readable(&tx, &self.store)?;
                }
                for op in &block.operations {
                    let path = op.path();
                    let root_socket_list = block.name == "open_sockets" && path == "open_sockets";
                    if (!root_socket_list && !path.starts_with(&format!("{}.", block.name))) || !seen.insert(path.to_string()) {
                        return Err(DeliveryError::new("DUPLICATE_OR_MISPLACED_PATH", "one operation per exact registered path").at(path));
                    }
                    let value = match op {
                        Operation::Set { value, .. } | Operation::Replace { value, .. } => {
                            Some(value)
                        }
                        Operation::Clear { .. } | Operation::Preserve { .. } => None,
                    };
                    let desc = inventory::validate_path(path, &package.purpose, value)?;
                    enforce_exact_unit_gate(&package, &s.node, path, &desc)?;
                    if matches!(op, Operation::Replace { .. })
                        && !matches!(desc.kind.as_str(), "array" | "object" | "json")
                    {
                        return Err(DeliveryError::new(
                            "INVALID_OPERATION",
                            "replace is only valid for collection or map fields",
                        )
                        .at(path));
                    }
                    if block.name == "mode" && desc.kind == "array" && matches!(op, Operation::Set { .. }) {
                        return Err(DeliveryError::new(
                            "INVALID_OPERATION", "voice word lists require a whole-list replacement",
                        ).at(path));
                    }
                    if let Operation::Set { value, .. } = op {
                        validate_set_value(path, value, self.store.allows_loopback_public())?;
                    }
                    if block.name == "weather" {
                        super::weather::validate_operation(op, self.store.allows_loopback_public())?;
                    }
                    let checked_value = discovery
                        .map(|block| block.checked_value(op, self.store.allows_loopback_public()))
                        .transpose()?.flatten();
                    let layer = if package.purpose == Purpose::FamilyPolicy {
                        &mut layers.family
                    } else {
                        &mut layers.node
                    };
                    match op {
                        Operation::Set { value, .. } | Operation::Replace { value, .. } => {
                            layer.insert(path.into(), checked_value.unwrap_or_else(|| value.clone()));
                        }
                        Operation::Clear { .. } => {
                            layer.remove(path);
                        }
                        Operation::Preserve { .. } => {}
                    }
                    requested.push((path.to_string(),op.action()));
                }
            }
            for binding in &package.bindings {
                let name = match binding { Binding::Preserve { binding_id } | Binding::RequireBinding { binding_id } => binding_id };
                if !inventory::BINDINGS.contains(&name.as_str()) { return Err(DeliveryError::new("UNKNOWN_BINDING", "unregistered target binding")); }
                if matches!(binding, Binding::RequireBinding { .. }) {
                    let bound = binding_available(&self.store, &tx, name)?;
                    if !bound { return Err(DeliveryError::new("BINDING_REQUIRED", format!("target-local binding {name} is missing"))); }
                }
            }
            let (after, origins) = resolved(&layers);
            validate_values(&after, self.store.allows_loopback_public())?;
            super::home::validate_directory_transition(&package, &resolved(&s.layers).0, &after)?;
            super::home::validate_family_directory(&package, &s.layers, &layers)?;
            for block in package.blocks.iter().filter_map(|block| DiscoveryBlock::named(&block.name)) {
                if matches!(block, DiscoveryBlock::OpenApi) {
                    super::openapi::review_layers(&tx, &self.store, &layers, &after)?;
                } else if matches!(block, DiscoveryBlock::OpenSockets) {
                    let desired = super::open_sockets::canonical(value_tree(&after, block.key()))?;
                    super::open_sockets::validate_namespace(&desired, competing_sockets.as_deref().unwrap_or_default())?;
                    super::open_sockets::check_bindings(&tx, &self.store, &desired)?;
                } else {
                    block.validate(value_tree(&after, block.key()), self.store.allows_loopback_public())?;
                }
            }
            if package.blocks.iter().any(|block| block.name == "mode") {
                super::mode::review_layers(&layers, &after)?;
            }
            if package.blocks.iter().any(|block| block.name == "weather") {
                super::weather::validate(value_tree(&after, "weather"), self.store.allows_loopback_public())?;
            }
            let mut diff = s.adopted;
            for (path, action) in requested {
                let (before_layer,after_layer)=if package.purpose==Purpose::FamilyPolicy {(&s.layers.family,&layers.family)} else {(&s.layers.node,&layers.node)};
                let layer_before = before_layer.get(&path);
                let layer_after = after_layer.get(&path);
                diff.push(Diff {
                    before: s.stored.get(&path).cloned().unwrap_or(Value::Null),
                    after: after.get(&path).cloned().unwrap_or(Value::Null),
                    origin: origins.get(&path).cloned().unwrap_or_else(|| "default".into()),
                    layer_before: layer_before.cloned(),
                    layer_after: layer_after.cloned(),
                    layer_before_null: layer_before.is_some_and(Value::is_null),
                    layer_after_null: layer_after.is_some_and(Value::is_null),
                    path,
                    action: action.into(),
                });
            }
            let package_hash = package.hash()?;
            let hash = digest(&json!({"package_hash":package_hash,"principal":principal,"epoch":s.epoch,"baseline":s.baseline,"layers":layers}))?;
            let plan_id = uuid()?;
            let expires = now.saturating_add(600_000);
            tx.execute("INSERT INTO config_plans(plan_id,principal,data_epoch,created_ms,expires_ms,plan_hash,expected_revision,baseline_hash,package_json,before_json,after_json,layers_json,diff_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![plan_id,principal,s.epoch as i64,now as i64,expires as i64,hash,s.revision as i64,s.baseline,serde_json::to_string(&package)?,serde_json::to_string(&s.stored)?,serde_json::to_string(&after)?,serde_json::to_string(&layers)?,serde_json::to_string(&diff)?])?;
            tx.commit()?;
            let blocked: Vec<Value> = migration_report
                .as_array()
                .into_iter()
                .flatten()
                .filter(|entry| entry["blocked"] == true)
                .cloned()
                .collect();
            Ok(json!({"plan_id":plan_id,"plan_hash":hash,"package_hash":package_hash,"expected_revision":s.revision,"expires_ms":expires,"diff":diff,"migration_report":migration_report,"blocked":blocked,"layer_mode":"partial"}))
        })
    }
    pub fn commit(&self, principal: &str, request: &Commit) -> Result<Value> {
        self.commit_at(principal, request, crate::id::now_ms(), None)
    }
    pub(crate) fn commit_at(
        &self,
        principal: &str,
        request: &Commit,
        now: u64,
        fail_after: Option<usize>,
    ) -> Result<Value> {
        if !valid_uuid(&request.application_id) {
            return Err(DeliveryError::new(
                "INVALID_APPLICATION_ID",
                "application_id must be a UUID",
            ));
        }
        // Replays must remain independent of later runtime reader failures.
        let replay = self.db(|c| {
            let row: Option<(String,u64,String)> = c.query_row("SELECT plan_hash,data_epoch,result_json FROM config_applications WHERE principal=?1 AND application_id=?2", params![principal,request.application_id], |r|Ok((r.get(0)?,r.get::<_,i64>(1)? as u64,r.get(2)?))).optional()?;
            if let Some((hash,epoch,receipt)) = row {
                if hash != request.plan_hash { return Err(DeliveryError::conflict("IDEMPOTENCY_CONFLICT", "application ID was used with a different plan")); }
                let current: u64 = c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r|r.get::<_,i64>(0)).map(|v|v as u64)?;
                if epoch != current { return Err(DeliveryError::conflict("PLAN_STALE", "application belongs to an old reset epoch")); }
                Ok(Some(serde_json::from_str::<Value>(&receipt)?))
            } else { Ok(None) }
        })?;
        if let Some(receipt) = replay {
            return Ok(receipt);
        }
        let before_baseline = self.db(|c| Ok(snapshot(c, &self.store)?.baseline))?;
        let before_readback = super::readback::capture(
            self.store.clone(),
            self.voice.as_deref(),
            self.open_sockets.as_ref(),
        )?;
        self.db(|c| {
            let tx = begin(c)?;
            let s = snapshot(&tx, &self.store)?;
            if let Some((hash, epoch, result)) = tx.query_row("SELECT plan_hash,data_epoch,result_json FROM config_applications WHERE principal=?1 AND application_id=?2",params![principal,request.application_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)? as u64,r.get::<_,String>(2)?))).optional()? {
                if hash != request.plan_hash { return Err(DeliveryError::conflict("IDEMPOTENCY_CONFLICT", "application ID was used with a different plan")); }
                if epoch != s.epoch { return Err(DeliveryError::conflict("PLAN_STALE", "application belongs to an old reset epoch")); }
                return Ok(serde_json::from_str(&result)?);
            }
            if s.baseline != before_baseline { return Err(DeliveryError::conflict("PLAN_STALE", "runtime reader baseline changed")); }
            let plan: Option<(String,u64,u64,String,u64,String,String,String,String,String,String)> = tx.query_row("SELECT principal,data_epoch,expires_ms,plan_hash,expected_revision,baseline_hash,package_json,before_json,after_json,layers_json,diff_json FROM config_plans WHERE plan_id=?1",[&request.plan_id],|r|Ok((r.get(0)?,r.get::<_,i64>(1)? as u64,r.get::<_,i64>(2)? as u64,r.get(3)?,r.get::<_,i64>(4)? as u64,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?))).optional()?;
            let Some((actor,epoch,expires,hash,revision,baseline,pkg,_before,after,layers,diff)) = plan else { return Err(DeliveryError::conflict("PLAN_STALE", "plan does not exist")); };
            if actor != principal { return Err(DeliveryError::conflict("PLAN_AUTHORITY_MISMATCH", "plan belongs to another authenticated token")); }
            if epoch != s.epoch || now >= expires || hash != request.plan_hash || revision != request.expected_revision || revision != s.revision || baseline != s.baseline {
                return Err(DeliveryError::conflict("PLAN_STALE", "revision, epoch, expiry, hash or target baseline changed"));
            }
            let package: Package = serde_json::from_str(&pkg)?;
            validate_envelope(&package, &s)?;
            for block in &package.blocks {
                for op in &block.operations {
                    let path = op.path();
                    let value = match op {
                        Operation::Set { value, .. } | Operation::Replace { value, .. } => Some(value),
                        Operation::Clear { .. } | Operation::Preserve { .. } => None,
                    };
                    let desc = inventory::validate_path(path, &package.purpose, value)?;
                    enforce_exact_unit_gate(&package, &s.node, path, &desc)?;
                }
            }
            let after_fields: Fields = serde_json::from_str(&after)?;
            let next_layers: Layers = serde_json::from_str(&layers)?;
            validate_values(&after_fields, self.store.allows_loopback_public())?;
            super::home::validate_directory_transition(&package, &resolved(&s.layers).0, &after_fields)?;
            super::home::validate_family_directory(&package, &s.layers, &next_layers)?;
            for block in package.blocks.iter().filter_map(|block| DiscoveryBlock::named(&block.name)) {
                if matches!(block, DiscoveryBlock::OpenApi) {
                    super::openapi::review_layers(&tx, &self.store, &next_layers, &after_fields)?;
                } else if matches!(block, DiscoveryBlock::OpenSockets) {
                    if self.open_sockets.is_none() {
                        return Err(DeliveryError::conflict("OPEN_SOCKETS_RUNTIME_UNAVAILABLE", "reviewed sockets require the actual Registry").at(block.key()));
                    }
                    super::open_sockets::require_enrolled_identity(&tx, &s.node)?;
                    let desired = super::open_sockets::canonical(value_tree(&after_fields, block.key()))?;
                    super::open_sockets::check_bindings(&tx, &self.store, &desired)?;
                } else {
                    block.validate(value_tree(&after_fields, block.key()), self.store.allows_loopback_public())?;
                }
            }
            if package.blocks.iter().any(|block| block.name == "mode") {
                if self.voice.is_none() {
                    return Err(DeliveryError::conflict("VOICE_RUNTIME_UNAVAILABLE", "voice policy requires the active Core runtime").at("mode"));
                }
                super::mode::require_readable(&tx)?;
                super::mode::review_layers(&next_layers, &after_fields)?;
            }
            if package.blocks.iter().any(|block| block.name == "weather") {
                super::weather::validate(value_tree(&after_fields, "weather"), self.store.allows_loopback_public())?;
            }
            let mut pending_blocks=BTreeSet::new();
            let mut retry=false;
            for &block in PUBLISHED_BLOCKS {
                if value_tree(&s.stored,block)!=value_tree(&after_fields,block) {
                    pending_blocks.insert(block.to_string());
                    let queued:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM config_outbox o JOIN config_applications a USING(principal,application_id) JOIN config_application_blocks b USING(principal,application_id) WHERE o.data_epoch=?1 AND a.revision=?2 AND b.block=?3 AND b.state='saved_pending')",params![epoch as i64,s.revision as i64,block],|r|r.get(0))?;
                    retry |= !queued;
                }
            }
            let changed = resolved(&s.layers).0 != after_fields || next_layers != s.layers || retry;
            let next = s.revision + u64::from(changed);
            let state = if changed { "saved_pending" } else { "unchanged" };
            let mut names = package.blocks.iter().map(|b|b.name.clone()).collect::<BTreeSet<_>>();
            if changed {names.extend(pending_blocks);}
            let blocks = names.iter().map(|name|json!({"name":name,"state":state})).collect::<Vec<_>>();
            let result = json!({"application_id":request.application_id,"actor":principal,"plan_id":request.plan_id,"plan_hash":hash,"config_revision":next,"revision":next,"data_epoch":epoch,"created_ms":now,"state":state,"layer_mode":"partial","purpose":package.purpose,"change_reason":package.change_reason,"target":{"household_id":s.household,"node_id":s.node,"enrolled":true},"blocks":blocks,"diff":serde_json::from_str::<Value>(&diff)?});
            let mut step = 0;
            let mut checkpoint = || -> Result<()> { step+=1; if fail_after==Some(step) { Err(DeliveryError::internal("injected transactional failure")) } else { Ok(()) } };
            tx.execute("UPDATE config_head SET applying=1 WHERE id=1", [])?; checkpoint()?;
            if changed { project(&tx,&s.stored,&after_fields,&mut checkpoint)?; }
            tx.execute("UPDATE config_head SET revision=?1,layers_json=?2,resolved_json=?3,applying=0 WHERE id=1",params![next as i64,layers,after])?; checkpoint()?;
            let before_snapshot=json!({"fields":s.stored,"layers":s.layers,"origins":s.origins,"readback":before_readback});
            let after_snapshot=json!({"fields":after_fields,"layers":serde_json::from_str::<Value>(&layers)?});
            tx.execute("INSERT INTO config_applications(application_id,principal,data_epoch,plan_hash,revision,created_ms,before_json,after_json,result_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![request.application_id,principal,epoch as i64,hash,next as i64,now as i64,before_snapshot.to_string(),after_snapshot.to_string(),result.to_string()])?; checkpoint()?;
            for name in &names { tx.execute("INSERT INTO config_application_blocks VALUES (?1,?2,?3,?4)",params![request.application_id,principal,name,state])?; checkpoint()?; }
            if changed {
                let mut payload=json!({"revision":next,"required_projection":state});
                if names.contains("persona") {
                    payload["persona"]=json!({"expected":s.stored[super::worker::PERSONA],"desired":after_fields[super::worker::PERSONA],"expected_revision_id":super::worker::revision_id(&tx)?,"state":"saved_pending","legacy":false});
                }
                for block in [
                    "services",
                    "weather",
                    "home",
                    "music",
                    "news",
                    "search",
                    "openapi",
                    "mode",
                    "open_sockets",
                    "reflex",
                    "node",
                    "audio_eq",
                    "device_led_configs",
                    "unit_acoustic_profiles",
                ] {
                    if names.contains(block) {
                        let kv_key = match block {
                            "mode" => crate::voice::VOICE_CFG_KEY,
                            _ => block,
                        };
                        payload[block] = json!({
                            "expected": value_tree(&s.stored, block),
                            "desired": value_tree(&after_fields, block),
                            "expected_raw": raw(&tx, kv_key)?,
                            "state": "saved_pending",
                            "legacy": false
                        });
                    }
                }
                tx.execute("INSERT INTO config_outbox VALUES (?1,?2,?3,?4,?5)",params![request.application_id,principal,epoch as i64,payload.to_string(),state])?; checkpoint()?;
            }
            tx.commit()?;
            Ok(result)
        })
    }
    /// Current evidence is deliberately separate from the immutable commit receipt.
    pub fn readback(&self, principal: &str, id: &str) -> Result<Value> {
        let receipt = self.application(principal, id)?;
        for _ in 0..3 {
            let baseline = self.db(|c| Ok(snapshot(c, &self.store)?.baseline))?;
            let observed = super::readback::capture(
                self.store.clone(),
                self.voice.as_deref(),
                self.open_sockets.as_ref(),
            )?;
            if let Some(evidence) =
                self.readback_observed(principal, id, &receipt, &baseline, observed)?
            {
                return Ok(evidence);
            }
        }
        Err(DeliveryError::conflict(
            "READBACK_CHANGED",
            "effective configuration changed during readback; retry",
        ))
    }

    fn readback_observed(
        &self,
        principal: &str,
        id: &str,
        receipt: &Value,
        baseline: &str,
        observed: Value,
    ) -> Result<Option<Value>> {
        self.db(|c| {
            let tx = begin(c)?;
            let s = snapshot(&tx, &self.store)?;
            let current = receipt["config_revision"] == s.revision && receipt["data_epoch"] == s.epoch;
            if current && baseline != s.baseline {
                // Native publication advances effective state at the same
                // desired revision. Recapture without reporting supersession.
                return Ok(None);
            }
            let after: String = tx.query_row("SELECT after_json FROM config_applications WHERE principal=?1 AND application_id=?2", params![principal,id], |r|r.get(0))?;
            let after: Value = serde_json::from_str(&after)?;
            let mut publications=PUBLISHED_BLOCKS.iter().map(|block|super::worker::evidence(&tx,principal,id,block)).collect::<Result<Vec<_>>>()?;
            publications.retain(|value|!value.is_null());
            if !current {
                for publication in &mut publications { publication["state"] = json!("superseded"); }
            } else if publications.iter().any(|item| item["block"] == "open_sockets" && item["state"] == "applied")
                && !super::open_sockets::live_node_matches(&tx, &s.node)?
            {
                for publication in publications.iter_mut().filter(|item| item["block"] == "open_sockets" && item["state"] == "applied") {
                    publication["state"] = json!("superseded");
                    publication["error"] = json!("OPEN_SOCKETS_ENROLLED_NODE_MISMATCH");
                }
            }
            let publication=publications.first().cloned().unwrap_or(Value::Null);
            let fields = receipt["diff"].as_array().into_iter().flatten().map(|diff| {
                let path = diff["path"].as_str().unwrap_or("");
                let block_publication=publications.iter().find(|value|value["block"]==path.split('.').next().unwrap_or(""));
                let state = if !current { "superseded" } else if path.starts_with("shared_memory.") { "pending_dependency" }
                    else if receipt["purpose"] == "family_policy" && diff["origin"] == "node" { "shadowed" }
                    else if let Some(value)=block_publication.filter(|value|value["state"]!="applied") {value["state"].as_str().unwrap_or("saved_pending")}
                    else if observed["fields"][path] != after["fields"][path] { "reader_mismatch" }
                    else { observed["field_states"][path].as_str().unwrap_or("not_consumed") };
                json!({"path":path,"state":state,"value":observed["fields"][path],"dependency":if path.starts_with("shared_memory.") {Some("#107")} else {None}})
            }).collect::<Vec<_>>();
            let mut evidence = json!({"initial_receipt":receipt,"observed_revision":s.revision,"data_epoch":s.epoch,"checked_ms":crate::id::now_ms(),"state":if current {"partial_readback"} else {"superseded"},"fields":fields,"scope":observed["scope"],"remote_health":"not_probed","publication_isolation":if publications.iter().any(|value|value["block"]!="persona" && value["isolation"]=="desired_until_worker") {"core_blocks"} else if publication["isolation"]=="desired_until_worker" {"persona"} else {"not_implemented"},"publication":publication,"publications":publications});
            if !observed["cue_readiness"].is_null() {
                evidence["cue_readiness"] = observed["cue_readiness"].clone();
            }
            let payload: Option<String> = tx.query_row("SELECT payload_json FROM config_outbox WHERE principal=?1 AND application_id=?2",params![principal,id],|r|r.get(0)).optional()?;
            if let Some(payload) = payload {
                let mut payload: Value = serde_json::from_str(&payload)?;
                payload["readback"] = evidence.clone();
                tx.execute("UPDATE config_outbox SET payload_json=?1 WHERE principal=?2 AND application_id=?3",params![payload.to_string(),principal,id])?;
            }
            tx.commit()?;
            Ok(Some(evidence))
        })
    }
    /// Rollback is another reviewed plan, never a direct historical write.
    /// Only the latest unchanged baseline is eligible in this narrow slice.
    pub fn rollback_preflight(
        &self,
        principal: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<Value> {
        let receipt = self.application(principal, id)?;
        let package = self.db(|c| {
            let s = snapshot(c, &self.store)?;
            if s.revision != expected_revision || receipt["config_revision"] != s.revision || receipt["data_epoch"] != s.epoch {
                return Err(DeliveryError::conflict("ROLLBACK_STALE", "only the latest unchanged application can be rolled back"));
            }
            let before: String = c.query_row("SELECT before_json FROM config_applications WHERE principal=?1 AND application_id=?2", params![principal,id], |r|r.get(0))?;
            let before: Value = serde_json::from_str(&before)?;
            let purpose: Purpose = serde_json::from_value(receipt["purpose"].clone())?;
            let layer = if purpose == Purpose::FamilyPolicy {"family"} else {"node"};
            let mut blocks: BTreeMap<String,Vec<Operation>> = BTreeMap::new();
            for diff in receipt["diff"].as_array().into_iter().flatten() {
                if diff["action"] == "preserve" || diff["action"] == "adopt_external_write" { continue; }
                let path = diff["path"].as_str().unwrap_or("");
                let duplex_absent_and_mode_verified = path.starts_with("mode.duplex_requested.")
                    && before["fields"].get(path).is_none()
                    && before["readback"]["field_states"]["mode.follow_window_ms"]
                        == "core_readback_verified";
                if (!duplex_absent_and_mode_verified
                    && !["core_readback_verified", "binding_name_readback"]
                        .iter()
                        .any(|state| before["readback"]["field_states"][path] == *state))
                    || before["readback"]["fields"][path] != before["fields"][path]
                {
                    return Err(DeliveryError::conflict("ROLLBACK_UNVERIFIED", "before value has no native reader proof in this slice").at(path));
                }
                let operation = match before["layers"][layer].get(path) {
                    Some(value) if value == &before["fields"][path] => {
                        if value.is_array() || value.is_object() {
                            Operation::Replace {path:path.into(),value:value.clone()}
                        } else {
                            Operation::Set {path:path.into(),value:value.clone()}
                        }
                    },
                    Some(_) => return Err(DeliveryError::conflict("ROLLBACK_UNVERIFIED", "shadowed layer value has no native reader proof").at(path)),
                    None => Operation::Clear {path:path.into()}
                };
                blocks.entry(path.split('.').next().unwrap_or("").into()).or_default().push(operation);
            }
            if blocks.is_empty() { return Err(DeliveryError::new("ROLLBACK_EMPTY", "no verified reversible changes")); }
            let schema: i64 = c.query_row("SELECT COALESCE(MAX(version),0) FROM schema_version", [], |r|r.get(0))?;
            Ok(Package {format:"home-mate-config".into(),version:2,package_id:uuid()?,package_revision:1,created_ms:crate::id::now_ms(),purpose,
                source:Source {node_id:s.node.clone(),config_revision:s.revision,core_version:crate::CORE_VERSION.into(),schema_version:schema as u64},
                target:Target {household_id:s.household,node_id:if layer=="family" {None} else {Some(s.node)}},
                compatibility:Compatibility {core_api_schema:2,gateway_protocol_min:0,gateway_protocol_max:0,required_capabilities:vec![]},
                change_reason:format!("Rollback application {id}"),blocks:blocks.into_iter().map(|(name,operations)|Block {name,schema_version:1,operations}).collect(),bindings:vec![],content_hash:None})
        })?;
        // preflight reads a fresh baseline; reject a race rather than issuing
        // a plan that silently reverses a later edit.
        let plan = self.preflight(principal, package, json!({"rollback_of":id}))?;
        if plan["expected_revision"] != expected_revision {
            return Err(DeliveryError::conflict(
                "ROLLBACK_STALE",
                "configuration changed during rollback preview",
            ));
        }
        Ok(plan)
    }
    pub fn applications(&self, principal: &str) -> Result<Value> {
        self.db(|c| Ok(json!({"applications":list(c,principal)?})))
    }
    pub fn application(&self, principal: &str, id: &str) -> Result<Value> {
        self.db(|c| {
            let result: Option<String> = c.query_row("SELECT result_json FROM config_applications WHERE principal=?1 AND application_id=?2",params![principal,id],|r|r.get(0)).optional()?;
            match result { Some(s)=>Ok(serde_json::from_str(&s)?),None=>Err(DeliveryError {status:404,..DeliveryError::new("APPLICATION_NOT_FOUND","no application for this principal")}) }
        })
    }
}

fn list(c: &Connection, principal: &str) -> Result<Vec<Value>> {
    let mut stmt = c.prepare("SELECT result_json FROM config_applications WHERE principal=?1 ORDER BY created_ms DESC,application_id DESC LIMIT 50")?;
    let rows = stmt
        .query_map([principal], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.iter()
        .map(|s| serde_json::from_str(s).map_err(Into::into))
        .collect()
}
fn enforce_exact_unit_gate(
    package: &Package,
    enrolled_node: &str,
    path: &str,
    desc: &inventory::FieldDescriptor,
) -> Result<()> {
    if desc.scope == "unit" {
        if package.purpose == Purpose::FamilyPolicy {
            return Err(DeliveryError::new(
                "FORBIDDEN_SCOPE",
                "unit-scoped fields cannot be applied via family_policy",
            )
            .at(path));
        }
        if package.target.node_id.as_deref() != Some(enrolled_node) {
            return Err(DeliveryError::new(
                "TARGET_MISMATCH",
                "unit-scoped fields require target.node_id == enrolled_node_id",
            )
            .at(path));
        }
        if package.purpose == Purpose::SameNodeRestore && package.source.node_id != enrolled_node {
            return Err(DeliveryError::new(
                "TARGET_MISMATCH",
                "unit-scoped same-node restore requires matching source node_id",
            )
            .at(path));
        }
        if let Some(rest) = path.strip_prefix("unit_acoustic_profiles.") {
            if let Some((dev, _)) = rest.rsplit_once('.') {
                if dev != enrolled_node {
                    return Err(DeliveryError::new(
                        "FORBIDDEN_SCOPE",
                        format!(
                            "unit acoustic profile for '{dev}' cannot be applied to enrolled node '{enrolled_node}'"
                        ),
                    )
                    .at(path));
                }
            }
        }
    }
    if let Some(rest) = path.strip_prefix("device_led_configs.") {
        if let Some((dev, _)) = rest.rsplit_once('.') {
            if dev != enrolled_node {
                return Err(DeliveryError::new(
                    "TARGET_MISMATCH",
                    format!("device LED config for '{dev}' does not match enrolled node '{enrolled_node}'"),
                )
                .at(path));
            }
        }
    }
    if let Some(dev) = path.strip_prefix("mode.duplex_requested.") {
        if dev != enrolled_node {
            return Err(DeliveryError::new(
                "TARGET_MISMATCH",
                format!(
                    "duplex_requested for '{dev}' does not match enrolled node '{enrolled_node}'"
                ),
            )
            .at(path));
        }
    }
    Ok(())
}

fn validate_envelope(package: &Package, s: &Snapshot) -> Result<()> {
    if s.household.is_empty() {
        return Err(DeliveryError::conflict(
            "ENROLLMENT_REQUIRED",
            "initialize this node before reviewing a package",
        ));
    }
    if package.format != "home-mate-config"
        || package.version != 2
        || !valid_uuid(&package.package_id)
        || package.package_revision == 0
        || package.change_reason.trim().is_empty()
        || package.change_reason.chars().count() > 1000
    {
        return Err(DeliveryError::new(
            "INVALID_PACKAGE",
            "unsupported format/version, invalid identity/revision or missing bounded reason",
        ));
    }
    if package.target.household_id != s.household
        || package.purpose != Purpose::FamilyPolicy
            && package.target.node_id.as_deref() != Some(s.node.as_str())
        || package.purpose == Purpose::FamilyPolicy && package.target.node_id.is_some()
    {
        return Err(DeliveryError::new(
            "TARGET_MISMATCH",
            "package purpose/identity does not match receiver enrollment",
        ));
    }
    if package.purpose == Purpose::SameNodeRestore && package.source.node_id != s.node {
        return Err(DeliveryError::new(
            "TARGET_MISMATCH",
            "same-node restore has a foreign source",
        ));
    }
    let allowed_caps = [
        "config_apply_v2",
        "config_block_kws",
        "config_block_audio_eq",
        "config_block_led",
        "config_block_duplex",
        "config_block_acoustics",
    ];
    if package.compatibility.core_api_schema != 2
        || package.compatibility.gateway_protocol_min > package.compatibility.gateway_protocol_max
        || !package
            .compatibility
            .required_capabilities
            .iter()
            .all(|cap| allowed_caps.contains(&cap.as_str()))
    {
        return Err(DeliveryError::new(
            "UNSUPPORTED_CAPABILITY",
            "unsupported schema or capability requirement",
        ));
    }
    if let Some(hash) = &package.content_hash {
        if !hash.is_empty() && *hash != package.hash()? {
            return Err(DeliveryError::new(
                "PACKAGE_HASH_MISMATCH",
                "canonical package hash does not match",
            ));
        }
    }
    if package.blocks.len() > 32
        || package
            .blocks
            .iter()
            .map(|b| b.operations.len())
            .sum::<usize>()
            > 256
    {
        return Err(DeliveryError::new(
            "PACKAGE_TOO_LARGE",
            "too many blocks/operations",
        ));
    }
    Ok(())
}
fn valid_uuid(id: &str) -> bool {
    super::model::valid_uuid(id)
}
pub(super) fn value_tree(fields: &Fields, root: &str) -> Value {
    if let Some(value) = fields.get(root) {
        return value.clone();
    }
    let mut out = json!({});
    for (path, value) in fields {
        if let Some(tail) = path.strip_prefix(&format!("{root}.")) {
            let mut cursor = &mut out;
            let parts = tail.split('.').collect::<Vec<_>>();
            for key in &parts[..parts.len() - 1] {
                cursor = cursor
                    .as_object_mut()
                    .unwrap()
                    .entry((*key).to_string())
                    .or_insert_with(|| json!({}));
            }
            cursor
                .as_object_mut()
                .unwrap()
                .insert(parts.last().unwrap().to_string(), value.clone());
        }
    }
    out
}
fn validate_values(fields: &Fields, allow_loopback: bool) -> Result<()> {
    let cfg: ServicesConfig = serde_json::from_value(value_tree(fields, "services"))?;
    for (name, service) in [
        ("llm", cfg.llm),
        ("llm_fallback", cfg.llm_fallback),
        ("asr", cfg.asr),
        ("tts", cfg.tts),
    ] {
        if [
            &service.provider,
            &service.base_url,
            &service.model,
            &service.thinking,
            &service.voice,
            &service.api_key_env,
        ]
        .iter()
        .any(|s| s.len() > 4096 || s.contains('\0'))
        {
            return Err(DeliveryError::new(
                "INVALID_SERVICE",
                "service field is too long or contains NUL",
            )
            .at(&format!("services.{name}")));
        }
        validate_url(&service.base_url, allow_loopback)
            .map_err(|e| e.at(&format!("services.{name}.base_url")))?;
        if !service.api_key_env.is_empty()
            && !service
                .api_key_env
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
        {
            return Err(DeliveryError::new(
                "INVALID_BINDING_NAME",
                "environment binding must be an identifier",
            )
            .at(&format!("services.{name}.api_key_env")));
        }
        if !service.is_configured() {
            continue;
        }
        if name.starts_with("llm") {
            if service.base_url.is_empty() || service.model.is_empty() {
                return Err(DeliveryError::new(
                    "INVALID_SERVICE",
                    "configured LLM requires base_url and model",
                )
                .at(&format!("services.{name}")));
            }
            crate::llm::deepseek::DeepSeekClient::with_thinking(
                &service.base_url,
                "",
                &service.thinking,
            )
            .map_err(|e| DeliveryError::new("INVALID_SERVICE", e.to_string()))?;
        } else if name == "asr" {
            crate::speech::asr_from_config(&service, None)
                .map_err(|e| DeliveryError::new("INVALID_SERVICE", e.to_string()))?;
        } else {
            crate::speech::tts_from_config(
                &service,
                Some("validation-placeholder-not-a-credential"),
            )
            .map_err(|e| DeliveryError::new("INVALID_SERVICE", e.to_string()))?;
        }
    }
    let shared: SharedMemoryConfig = serde_json::from_value(value_tree(fields, "shared_memory"))?;
    validate_url(&shared.nas_base_url, allow_loopback)
        .map_err(|e| e.at("shared_memory.nas_base_url"))?;
    shared
        .validate()
        .map_err(|e| DeliveryError::new("INVALID_SHARED_MEMORY", e))?;
    if (shared.enabled || shared.upload_enabled) && shared.nas_base_url.trim().is_empty() {
        return Err(DeliveryError::new(
            "INVALID_SHARED_MEMORY",
            "enabled sharing/upload requires a NAS URL",
        ));
    }
    if fields["persona.agent_md.content"]
        .as_str()
        .unwrap()
        .chars()
        .count()
        > crate::agent_md::MAX_AGENT_MD_CHARS
    {
        return Err(DeliveryError::new(
            "PERSONA_TOO_LONG",
            "persona exceeds supported character bound",
        ));
    }
    let eq_val = value_tree(fields, "audio_eq");
    if eq_val.as_object().is_some_and(|o| !o.is_empty()) {
        let eq: crate::config::AudioEqConfig = serde_json::from_value(eq_val)?;
        eq.validate()
            .map_err(|e| DeliveryError::new("INVALID_AUDIO_EQ", e).at("audio_eq"))?;
    }
    let led_val = value_tree(fields, "device_led_configs");
    if let Some(map) = led_val.as_object() {
        for (dev, v) in map {
            let cfg: crate::config::DeviceLedConfig = serde_json::from_value(v.clone())?;
            cfg.validate().map_err(|e| {
                DeliveryError::new("INVALID_LED_CONFIG", e.to_string())
                    .at(&format!("device_led_configs.{dev}"))
            })?;
        }
    }
    let acoustics_val = value_tree(fields, "unit_acoustic_profiles");
    if let Some(map) = acoustics_val.as_object() {
        for (dev, v) in map {
            let prof: crate::config::UnitAcousticProfile = serde_json::from_value(v.clone())?;
            prof.validate().map_err(|e| {
                DeliveryError::new("INVALID_ACOUSTIC_PROFILE", e.to_string())
                    .at(&format!("unit_acoustic_profiles.{dev}"))
            })?;
            if prof.mix_gain > 2.0 {
                return Err(DeliveryError::new(
                    "INVALID_ACOUSTIC_PROFILE",
                    format!("mix_gain {} exceeds Gateway max 2.0", prof.mix_gain),
                )
                .at(&format!("unit_acoustic_profiles.{dev}.mix_gain")));
            }
        }
    }
    Ok(())
}
fn validate_set_value(path: &str, value: &Value, allow_loopback: bool) -> Result<()> {
    if let Some(text) = value.as_str() {
        let max = if path == "persona.agent_md.content" {
            crate::agent_md::MAX_AGENT_MD_CHARS
        } else {
            4096
        };
        if text.chars().count() > max || text.contains('\0') {
            return Err(DeliveryError::new(
                "INVALID_FIELD_VALUE",
                "field exceeds its character bound or contains NUL",
            )
            .at(path));
        }
        if path.ends_with(".base_url") || path == "shared_memory.nas_base_url" {
            validate_url(text, allow_loopback).map_err(|e| e.at(path))?;
        }
        if path.ends_with(".api_key_env")
            && !text.is_empty()
            && !text
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
        {
            return Err(DeliveryError::new(
                "INVALID_BINDING_NAME",
                "environment binding must be an identifier",
            )
            .at(path));
        }
    }
    Ok(())
}
pub(crate) fn portable_service_url(value: &str) -> bool {
    validate_url(value, true).is_ok()
}

fn validate_url(value: &str, allow_loopback: bool) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let url = crate::tool::http::validate_outbound_url(
        value,
        crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
    )
    .map_err(|e| match e {
        crate::tool::http::UrlPolicyError::UserInfoForbidden => DeliveryError::new(
            "INVALID_SERVICE_URL",
            "HTTP(S) URL cannot embed credentials, auth query parameters or fragments",
        ),
        _ => DeliveryError::new("INVALID_SERVICE_URL", format!("invalid service URL: {e}")),
    })?;
    let sensitive = [
        "token",
        "api_key",
        "apikey",
        "access_token",
        "key",
        "secret",
        "password",
        "authorization",
        "signature",
    ];
    if url.fragment().is_some()
        || url
            .query_pairs()
            .any(|(key, _)| sensitive.contains(&key.to_ascii_lowercase().as_str()))
    {
        return Err(DeliveryError::new(
            "INVALID_SERVICE_URL",
            "HTTP(S) URL cannot embed credentials, auth query parameters or fragments",
        ));
    }
    Ok(())
}
fn project(
    tx: &Transaction<'_>,
    before: &Fields,
    after: &Fields,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for key in ["shared_memory"] {
        let old = value_tree(before, key);
        let new = value_tree(after, key);
        if old != new {
            let mut persisted = match raw(tx, key)? {
                Some(text) => serde_json::from_str::<Value>(&text)?,
                None => json!({}),
            };
            merge_known(&mut persisted, &new);
            tx.execute("INSERT INTO config_kv(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,persisted.to_string()])?;
            checkpoint()?;
        }
    }
    Ok(())
}
pub(super) fn merge_known(target: &mut Value, source: &Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            match target.get_mut(key) {
                Some(old) if old.is_object() && value.is_object() => merge_known(old, value),
                _ => {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_blocks_link_local_metadata_and_loopback_unless_opted_in() {
        assert!(validate_url("http://169.254.169.254/", false).is_err());
        assert!(validate_url("http://169.254.169.254/latest/meta-data/", false).is_err());
        assert!(validate_url("http://[fe80::1]/", false).is_err());
        assert!(validate_url("http://metadata.google.internal/", false).is_err());
        assert!(validate_url("http://100.100.100.200/latest/meta-data/", false).is_err());
        assert!(validate_url("http://user:pass@192.168.1.10:8123/", false).is_err());
        assert!(validate_url("http://192.168.1.10:8123/api", false).is_ok());
        assert!(validate_url("https://api.example.com/v1", false).is_ok());
        assert!(validate_url("http://127.0.0.1:8080/mock", false).is_err());
        assert!(validate_url("http://localhost:8080/mock", false).is_err());
        assert!(validate_url("http://[::1]:8080/mock", false).is_err());
        assert!(validate_url("http://127.0.0.1:8080/mock", true).is_ok());
    }
}
