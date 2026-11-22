//! `releases/state.json`: the durable record the console reads.
//!
//! Written only by `hm-release` (installs/activations) and by the core's boot
//! guard (which appends a `boot-guard.json` sibling instead of editing this
//! file, so two writers never race).

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const STATE_SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseRef {
    pub id: String,
    pub serial: u64,
    pub version: String,
    #[serde(default)]
    pub build_id: String,
    #[serde(default)]
    pub firmware_version: String,
    #[serde(default)]
    pub firmware_build_id: String,
    #[serde(default)]
    pub gateway_version: String,
    #[serde(default)]
    pub console_index: String,
    #[serde(default)]
    pub model_sha256: String,
    /// Device model the release targets (`/mode` devices[].model).
    #[serde(default)]
    pub model: String,
    pub installed_ms: u64,
    #[serde(default)]
    pub activated_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DbWork {
    #[serde(default)]
    pub snapshot: Option<String>,
    /// The core's `ms` at snapshot time. The boot guard compares it with the
    /// schema sidecar's `ms`, so "was the DB opened after the snapshot" does
    /// not depend on file mtimes (WAL writes leave the DB file behind).
    #[serde(default)]
    pub snapshot_ms: Option<u64>,
    #[serde(default)]
    pub schema_before: Option<i64>,
    #[serde(default)]
    pub schema_after: Option<i64>,
    #[serde(default)]
    pub restored: bool,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BootstrapWork {
    #[serde(default)]
    pub web_aside: Option<String>,
    #[serde(default)]
    pub model_aside: Option<String>,
    #[serde(default)]
    pub restored: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LastRun {
    /// install | activate | rollback | boot_guard
    pub kind: String,
    /// running | ok | failed | rolling_back | rolled_back | rollback_failed
    pub status: String,
    #[serde(default)]
    pub target: Option<ReleaseRef>,
    #[serde(default)]
    pub from: Option<ReleaseRef>,
    /// The release that is active after this operation (audit: "activated
    /// to", not just "from").
    #[serde(default)]
    pub to: Option<ReleaseRef>,
    pub started_ms: u64,
    #[serde(default)]
    pub ended_ms: u64,
    #[serde(default)]
    pub criteria: Vec<Check>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub db: Option<DbWork>,
    #[serde(default)]
    pub bootstrap: Option<BootstrapWork>,
    #[serde(default)]
    pub pid: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstallerInfo {
    pub version: String,
    pub binary: String,
    pub updated_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct State {
    #[serde(default = "schema_default")]
    pub schema: u32,
    #[serde(default)]
    pub installer: Option<InstallerInfo>,
    #[serde(default)]
    pub active: Option<ReleaseRef>,
    #[serde(default)]
    pub previous: Option<ReleaseRef>,
    #[serde(default)]
    pub last: Option<LastRun>,
    #[serde(default)]
    pub history: Vec<LastRun>,
}

fn schema_default() -> u32 {
    STATE_SCHEMA
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            installer: None,
            active: None,
            previous: None,
            last: None,
            history: Vec::new(),
        }
    }
}

impl State {
    /// Missing file = first boot (no release ever installed): defaults.
    /// Corrupt JSON is an error — the caller must not silently forget which
    /// release is active.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{}: invalid state.json: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        write_atomic(path, &bytes)
    }

    /// Keep the last N runs, newest first, bounded so the file stays small.
    pub fn record(&mut self, run: LastRun) {
        self.history.insert(0, run.clone());
        self.history.truncate(10);
        self.last = Some(run);
    }

    pub fn active_serial(&self) -> Option<u64> {
        self.active.as_ref().map(|r| r.serial)
    }

    /// Release ids that must survive pruning: active, previous, and a release
    /// that was installed but not activated yet (otherwise activating or
    /// rolling back another release would delete the package the operator is
    /// about to switch to).
    pub fn keep_ids(&self) -> Vec<String> {
        let mut keep = Vec::new();
        if let Some(a) = &self.active {
            keep.push(a.id.clone());
        }
        if let Some(p) = &self.previous {
            keep.push(p.id.clone());
        }
        if let Some(last) = &self.last {
            if last.kind == "install" && last.status == "ok" {
                if let Some(t) = &last.target {
                    if !keep.contains(&t.id) {
                        keep.push(t.id.clone());
                    }
                }
            }
        }
        keep
    }
}

/// Write `bytes` to `path` atomically (same-directory temp + fsync + rename).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        std::process::id()
    ));
    {
        use std::io::Write;
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("sync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

pub fn release_ref(manifest: &crate::manifest::Manifest, installed_ms: u64) -> ReleaseRef {
    ReleaseRef {
        id: manifest.release_id(),
        serial: manifest.release.serial,
        version: manifest.release.version.clone(),
        build_id: manifest.components.core.build_id.clone(),
        firmware_version: manifest.components.firmware.version.clone(),
        firmware_build_id: manifest.components.firmware.build_id.clone(),
        gateway_version: manifest.components.firmware.gateway_version.clone(),
        console_index: manifest.components.console.index.clone(),
        model_sha256: manifest.components.model.sha256.clone(),
        model: manifest.target.model.clone(),
        installed_ms,
        activated_ms: 0,
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
