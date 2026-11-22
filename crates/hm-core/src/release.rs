//! Release bookkeeping owned by the core: the management view, spawning the
//! installer, and the boot guard that runs **before** the DB is opened.
//!
//! The core never verifies signatures and never writes activation pointers —
//! that is `hm-release`'s job (docs/d/m8a-design-20261113.md §1). What the
//! core owns here is what it alone can know: the DB snapshot/restore and the
//! `/status` build identity the health gate compares.

use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Machine-local token file shared between `hm-core` and on-device `hm-release` (#231).
pub const LOCAL_TOKEN_FILE: &str = "local-release.token";

pub fn local_token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCAL_TOKEN_FILE)
}

/// Ensure `<data_dir>/local-release.token` exists with mode `0600` and return its token.
pub fn ensure_local_token(data_dir: &Path) -> Result<String, String> {
    let path = local_token_path(data_dir);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let trimmed = raw.trim();
        if trimmed.len() >= 32 && trimmed.bytes().all(|b| b.is_ascii_graphic()) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            return Ok(trimmed.to_string());
        }
    }
    std::fs::create_dir_all(data_dir).map_err(|e| format!("mkdir {}: {e}", data_dir.display()))?;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("urandom: {e}"))?;
    let mut hex = String::with_capacity(68);
    hex.push_str("hmr_");
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    let tmp = data_dir.join(format!(".{LOCAL_TOKEN_FILE}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, format!("{hex}\n"))
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(hex)
}

/// Constant-time verification of a presented local release token against `<data_dir>/local-release.token`.
pub fn verify_local_token(data_dir: &Path, presented: &str) -> bool {
    let presented = presented.trim();
    if presented.len() < 32 {
        return false;
    }
    let Ok(expected) = std::fs::read_to_string(local_token_path(data_dir)) else {
        return false;
    };
    let a = expected.trim().as_bytes();
    let b = presented.as_bytes();
    if a.len() < 32 || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `/data/hm/releases` (sibling of the data dir). `HM_RELEASES_DIR` exists for
/// host tests; the device uses the derived path.
pub fn releases_dir(data_dir: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("HM_RELEASES_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    match data_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("releases"),
        _ => data_dir.join("releases"),
    }
}

/// `/data/hm/bin/hm-release`, or `HM_RELEASE_TOOL` on a host.
pub fn tool_path(data_dir: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("HM_RELEASE_TOOL") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    match data_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("bin").join("hm-release"),
        _ => PathBuf::from("hm-release"),
    }
}

/// `state.json` as written by `hm-release` (null until the first install).
pub fn state_json(data_dir: &Path) -> Value {
    let path = releases_dir(data_dir).join("state.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// Last `lines` lines of the bounded installer log (never whole history).
pub fn log_tail(data_dir: &Path, lines: usize) -> Vec<String> {
    let path = releases_dir(data_dir).join("installer.log");
    let Ok(mut f) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let mut text = String::new();
    // The log is capped at ~1 MiB by `hm-release`; read the tail only.
    let _ = f.read_to_string(&mut text);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines.clamp(1, 500));
    all[start..].iter().map(|s| s.to_string()).collect()
}

/// Spawn `hm-release` detached: it must survive the core restart it triggers.
/// stdout/stderr go to the bounded installer log; the pid is only a receipt.
pub fn spawn_tool(data_dir: &Path, args: &[String]) -> Result<u32, String> {
    use std::process::{Command, Stdio};
    let tool = tool_path(data_dir);
    if !tool.is_file() {
        return Err(format!(
            "release installer not found at {} — bootstrap it first (deploy/release/README.md)",
            tool.display()
        ));
    }
    let dir = releases_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let log = dir.join("installer.log");
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("open {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| format!("clone log: {e}"))?;
    let mut cmd = Command::new(&tool);
    cmd.args(args).stdin(Stdio::null()).stdout(out).stderr(err);
    if let Ok(token) = ensure_local_token(data_dir) {
        cmd.env("HM_RELEASE_TOKEN", token);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", tool.display()))?;
    Ok(child.id())
}

/// Whether an activation/install is currently recorded as running **and still
/// alive**. A power cut leaves `status: running` behind; that must not lock
/// the console out forever, so the recorded pid is checked. The installer's
/// own flock remains the hard guard.
pub fn busy(data_dir: &Path) -> bool {
    let state = state_json(data_dir);
    let last = match state.get("last") {
        Some(l) => l,
        None => return false,
    };
    let status = last.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if status != "running" && status != "rolling_back" {
        return false;
    }
    let pid = last.get("pid").and_then(|p| p.as_u64()).unwrap_or(0);
    // The device has /proc and can answer "is that pid still alive?". A host
    // test (or a platform without /proc) falls back to the age rule below.
    if pid > 0 && Path::new("/proc").is_dir() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    // No pid, or no /proc: a very recent run counts as live.
    let started = last.get("started_ms").and_then(|s| s.as_u64()).unwrap_or(0);
    now_ms().saturating_sub(started) < 10 * 60 * 1000
}

fn now_ms() -> u64 {
    crate::id::now_ms()
}

fn sidecar_path(data_dir: &Path) -> PathBuf {
    data_dir.join("home-mate.schema")
}

/// Write the applied schema + build identity. Read by `hm-release` before a
/// rollback (it has no SQLite dependency) and by the boot guard.
pub fn write_schema_sidecar(
    data_dir: &Path,
    schema: i64,
    build_id: &str,
    restored_from: Option<&str>,
) -> Result<(), String> {
    let value = json!({
        "schema": schema,
        "build": build_id,
        "ms": now_ms(),
        "restored_from": restored_from,
    });
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    let path = sidecar_path(data_dir);
    let tmp = data_dir.join(format!(".home-mate.schema.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

/// Restore `snapshot` over the live DB and drop the stale WAL/SHM. Called
/// before `Store::open`, so no process holds the file.
fn restore(db: &Path, snapshot: &Path) -> Result<(), String> {
    let dir = db.parent().ok_or("db has no parent")?;
    let tmp = dir.join(format!(".home-mate.db.restore-{}", std::process::id()));
    std::fs::copy(snapshot, &tmp).map_err(|e| format!("copy snapshot: {e}"))?;
    // fsync the copy before it replaces the live file.
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tmp)
        .map_err(|e| format!("open {}: {e}", tmp.display()))?;
    f.sync_all()
        .map_err(|e| format!("sync {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, db).map_err(|e| format!("rename restore: {e}"))?;
    for suffix in ["-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", db.display(), suffix));
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

fn record_boot_guard(dir: &Path, record: &Value) {
    let path = dir.join("boot-guard.json");
    if let Ok(bytes) = serde_json::to_vec_pretty(record) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Runs before the store is opened. Two cases:
///
/// 1. `restore-on-boot.json` — a deliberate rollback decided the DB must go
///    back to the pre-activation snapshot (schema advanced). Restore when
///    this build is the one the marker was written for.
/// 2. A power cut / supervisor swap during an activation (`last.status ==
///    "running"` for a different build) touched the DB after the snapshot:
///    restore before opening, so an old core never reads a newer schema.
pub fn boot_guard(data_dir: &Path, build_id: &str) -> Option<Value> {
    let dir = releases_dir(data_dir);
    let db = data_dir.join("home-mate.db");

    let marker_path = dir.join("restore-on-boot.json");
    if let Ok(raw) = std::fs::read(&marker_path) {
        let v: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let for_build = v.get("for_build").and_then(|b| b.as_str()).unwrap_or("");
        let snapshot = v.get("snapshot").and_then(|s| s.as_str()).unwrap_or("");
        let schema = v.get("schema").and_then(|s| s.as_i64()).unwrap_or(0);
        let why = v.get("why").and_then(|s| s.as_str()).unwrap_or("rollback");
        if !for_build.is_empty() && for_build != build_id {
            // Another build will handle it; do not restore under its feet.
            return None;
        }
        let mut record = json!({
            "at_ms": now_ms(), "build_id": build_id, "snapshot": snapshot,
            "schema": schema, "restored": false, "why": why, "source": "restore-on-boot",
        });
        if snapshot.is_empty() || !Path::new(snapshot).is_file() {
            record["error"] = json!("snapshot file is missing");
        } else {
            match restore(&db, Path::new(snapshot)) {
                Ok(()) => {
                    record["restored"] = json!(true);
                    let _ = write_schema_sidecar(data_dir, schema, build_id, Some(snapshot));
                    let _ = std::fs::remove_file(&marker_path);
                }
                Err(e) => record["error"] = json!(e),
            }
        }
        record_boot_guard(&dir, &record);
        return Some(record);
    }

    // Power-cut guard.
    let state = state_json(data_dir);
    let last = state.get("last")?;
    let status = last.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let target_build = last
        .get("target")
        .and_then(|t| t.get("build_id"))
        .and_then(|b| b.as_str())
        .unwrap_or("");
    let snapshot = last
        .get("db")
        .and_then(|d| d.get("snapshot"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    if status != "running"
        || target_build.is_empty()
        || target_build == build_id
        || snapshot.is_empty()
    {
        return None;
    }
    // M8b residue: prefer the recorded timestamps over file mtimes. SQLite
    // keeps its writes in the `-wal` file, so the main DB file's mtime can
    // stand still across a whole activation — the old comparison then decided
    // "not touched" and let an old core open a newer schema. Both numbers
    // below are written by the core itself (the sidecar when a core last
    // opened the DB, `snapshot_ms` when the snapshot was taken), so they share
    // one clock. The mtime comparison stays as the fallback for state files
    // written before `snapshot_ms` existed.
    let snapshot_ms = last
        .get("db")
        .and_then(|d| d.get("snapshot_ms"))
        .and_then(|v| v.as_u64());
    let sidecar_ms = std::fs::read_to_string(sidecar_path(data_dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("ms").and_then(|m| m.as_u64()));
    let touched = match (sidecar_ms, snapshot_ms) {
        (Some(opened), Some(taken)) => opened > taken,
        _ => {
            let snapshot_mtime = std::fs::metadata(snapshot).and_then(|m| m.modified()).ok();
            let db_mtime = std::fs::metadata(&db).and_then(|m| m.modified()).ok();
            match (snapshot_mtime, db_mtime) {
                (Some(s), Some(d)) => d > s,
                _ => false,
            }
        }
    };
    if !touched {
        return None;
    }
    let schema = last
        .get("db")
        .and_then(|d| d.get("schema_before"))
        .and_then(|s| s.as_i64())
        .unwrap_or(0);
    let mut record = json!({
        "at_ms": now_ms(), "build_id": build_id, "snapshot": snapshot,
        "schema": schema, "restored": false, "source": "power-cut-guard",
        "why": "booted a different build while an activation was running",
    });
    match restore(&db, Path::new(snapshot)) {
        Ok(()) => {
            record["restored"] = json!(true);
            let _ = write_schema_sidecar(data_dir, schema, build_id, Some(snapshot));
        }
        Err(e) => record["error"] = json!(e),
    }
    record_boot_guard(&dir, &record);
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hm-core-release-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        dir
    }

    #[test]
    fn a_rollback_marker_restores_the_snapshot_and_clears_itself() {
        let root = temp("restore");
        let data = root.join("data");
        std::fs::write(data.join("home-mate.db"), b"new-schema-db").unwrap();
        std::fs::write(data.join("home-mate.db-wal"), b"stale-wal").unwrap();
        let snap = data.join("snap.db");
        std::fs::write(&snap, b"pre-activation-db").unwrap();
        let releases = root.join("releases");
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::write(
            releases.join("restore-on-boot.json"),
            json!({"snapshot": snap.display().to_string(), "schema": 5, "for_build": "oldbuild", "why": "rollback"})
                .to_string(),
        )
        .unwrap();

        let record = boot_guard(&data, "oldbuild").expect("guard ran");
        assert_eq!(record["restored"], true);
        assert_eq!(
            std::fs::read(data.join("home-mate.db")).unwrap(),
            b"pre-activation-db"
        );
        assert!(!data.join("home-mate.db-wal").exists());
        assert!(!releases.join("restore-on-boot.json").exists());
        let guard: Value =
            serde_json::from_slice(&std::fs::read(releases.join("boot-guard.json")).unwrap())
                .unwrap();
        assert_eq!(guard["restored"], true);
        let sidecar: Value =
            serde_json::from_slice(&std::fs::read(data.join("home-mate.schema")).unwrap()).unwrap();
        assert_eq!(sidecar["schema"], 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_marker_for_another_build_is_left_alone() {
        let root = temp("otherbuild");
        let data = root.join("data");
        std::fs::write(data.join("home-mate.db"), b"live").unwrap();
        let snap = data.join("snap.db");
        std::fs::write(&snap, b"snapshot").unwrap();
        let releases = root.join("releases");
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::write(
            releases.join("restore-on-boot.json"),
            json!({"snapshot": snap.display().to_string(), "schema": 5, "for_build": "oldbuild"})
                .to_string(),
        )
        .unwrap();
        assert!(boot_guard(&data, "someotherbuild").is_none());
        assert_eq!(std::fs::read(data.join("home-mate.db")).unwrap(), b"live");
        assert!(releases.join("restore-on-boot.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_power_cut_pending_activation_restores_only_when_the_db_was_touched() {
        let root = temp("powercut");
        let data = root.join("data");
        let db = data.join("home-mate.db");
        std::fs::write(&db, b"live").unwrap();
        let snap = data.join("snap.db");
        std::fs::write(&snap, b"snapshot").unwrap();
        // Make the snapshot older than the DB on purpose.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let f = std::fs::File::options().write(true).open(&snap).unwrap();
        f.set_modified(old).unwrap();
        let releases = root.join("releases");
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::write(
            releases.join("state.json"),
            json!({"last": {"status": "running", "target": {"build_id": "newbuild"}, "db": {"snapshot": snap.display().to_string(), "schema_before": 5}}}).to_string(),
        )
        .unwrap();
        let record = boot_guard(&data, "oldbuild").expect("power-cut guard ran");
        assert_eq!(record["restored"], true);
        assert_eq!(std::fs::read(&db).unwrap(), b"snapshot");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// M8b residue: SQLite keeps its writes in the `-wal` file, so a DB that
    /// was genuinely written during an activation can still carry a stale
    /// mtime. The recorded timestamps (snapshot + sidecar) are the evidence.
    #[test]
    fn the_sidecar_shows_a_power_cut_the_mtimes_hide() {
        let root = temp("powercut-sidecar");
        let data = root.join("data");
        let db = data.join("home-mate.db");
        std::fs::write(&db, b"live").unwrap();
        let snap = data.join("snap.db");
        std::fs::write(&snap, b"snapshot").unwrap();
        // Deliberately the wrong way round for mtime: the DB looks OLDER.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        let f = std::fs::File::options().write(true).open(&db).unwrap();
        f.set_modified(old).unwrap();
        let taken: u64 = 1_700_000_000_000;
        std::fs::write(
            sidecar_path(&data),
            json!({"schema": 5, "build": "newbuild", "ms": taken + 5_000}).to_string(),
        )
        .unwrap();
        let releases = root.join("releases");
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::write(
            releases.join("state.json"),
            json!({"last": {"status": "running", "target": {"build_id": "newbuild"},
                   "db": {"snapshot": snap.display().to_string(), "snapshot_ms": taken,
                          "schema_before": 5}}})
            .to_string(),
        )
        .unwrap();
        let record = boot_guard(&data, "oldbuild")
            .expect("the sidecar proved a core ran after the snapshot");
        assert_eq!(record["restored"], true);
        assert_eq!(std::fs::read(&db).unwrap(), b"snapshot");

        // The other direction: the sidecar predates the snapshot, so nothing
        // ran against this DB after it was taken — leave it alone even though
        // the mtimes say "newer".
        std::fs::write(&db, b"live").unwrap();
        let f = std::fs::File::options().write(true).open(&db).unwrap();
        f.set_modified(std::time::SystemTime::now()).unwrap();
        std::fs::write(
            sidecar_path(&data),
            json!({"schema": 5, "build": "oldbuild", "ms": taken - 5_000}).to_string(),
        )
        .unwrap();
        std::fs::write(
            releases.join("state.json"),
            json!({"last": {"status": "running", "target": {"build_id": "newbuild"},
                   "db": {"snapshot": snap.display().to_string(), "snapshot_ms": taken,
                          "schema_before": 5}}})
            .to_string(),
        )
        .unwrap();
        let _ = std::fs::remove_file(releases.join("boot-guard.json"));
        assert!(boot_guard(&data, "oldbuild").is_none());
        assert_eq!(std::fs::read(&db).unwrap(), b"live");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_completed_activation_is_not_touched_on_the_next_boot() {
        let root = temp("done");
        let data = root.join("data");
        std::fs::write(data.join("home-mate.db"), b"live").unwrap();
        let releases = root.join("releases");
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::write(
            releases.join("state.json"),
            json!({"last": {"status": "ok", "target": {"build_id": "newbuild"}, "db": {"snapshot": "/nonexistent"}}}).to_string(),
        )
        .unwrap();
        assert!(boot_guard(&data, "newbuild").is_none());
        assert_eq!(std::fs::read(data.join("home-mate.db")).unwrap(), b"live");
        let _ = std::fs::remove_dir_all(&root);
    }
}
