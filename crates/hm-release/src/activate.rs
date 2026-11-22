//! Activation: the single writer of `releases/current`, the component slots,
//! the console symlink and `state.json`.
//!
//! Sequence (design: docs/d/m8a-design-20261113.md §5): re-verify the release
//! directory → device preflight → DB snapshot → record `running` → adopt the
//! current web/model (bootstrap) → write inactive slots → flip pointers →
//! restart firmware, then core → health confirm. Any failure flips everything
//! back and confirms the previous release, or reports `rollback_failed`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::device::{
    self, flip_symlink, inactive_slot, read_current, validate_elf, wait_until,
    write_slot_from_file, FileLock, Paths, AUDIO_SLOTS, CORE_SLOTS,
};
use crate::health::{self, Expect, Report};
use crate::install::verify_release_dir;
use crate::manifest::Manifest;
use crate::state::{self, BootstrapWork, Check, DbWork, LastRun, ReleaseRef, State};

fn budget(default_secs: u64, env: &str) -> Duration {
    let secs = std::env::var(env)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

fn health_budget() -> Duration {
    budget(90, "HM_RELEASE_HEALTH_BUDGET_S")
}
fn firmware_budget() -> Duration {
    budget(60, "HM_RELEASE_FIRMWARE_BUDGET_S")
}
fn rollback_budget() -> Duration {
    budget(60, "HM_RELEASE_ROLLBACK_BUDGET_S")
}
fn core_budget() -> Duration {
    budget(45, "HM_RELEASE_CORE_BUDGET_S")
}

pub fn run(paths: &Paths, id: Option<String>, rollback: bool, reason: &str) -> Result<i32, String> {
    let _lock = FileLock::acquire(&paths.lock_path())?;
    paths.ensure_dirs()?;
    let mut st = State::load(&paths.state_path())?;
    // Invariant: `previous` is a different release. A self-reference can only
    // come from the re-activation defect fixed in 07172b1; repair it here so a
    // corrupt state does not make "roll back" a no-op forever.
    if repair_previous(&mut st) {
        device::log_line(
            &paths.log_path(),
            "state: cleared a self-referential previous release",
        );
        st.save(&paths.state_path())?;
    }
    let from = st.active.clone();

    let (target_id, explicit_rollback) = if rollback {
        let prev = st
            .previous
            .clone()
            .ok_or("no previous release recorded; cannot roll back")?;
        (prev.id, true)
    } else if let Some(id) = id {
        (id, false)
    } else {
        (
            from.as_ref()
                .ok_or("no active release; pass --id <release>")?
                .id
                .clone(),
            false,
        )
    };

    let dir = paths.releases().join(&target_id);
    if !dir.is_dir() {
        return Err(format!(
            "release {target_id} is not installed at {}",
            dir.display()
        ));
    }
    let manifest = verify_release_dir(&dir)?;
    let now = state::now_ms();
    let mut target = state::release_ref(&manifest, now);
    if let Some(known) = st
        .previous
        .as_ref()
        .filter(|p| p.id == target_id)
        .or_else(|| from.as_ref().filter(|f| f.id == target_id))
    {
        target.installed_ms = known.installed_ms;
        target.activated_ms = known.activated_ms;
    }
    let kind = if explicit_rollback || from.as_ref().map_or(false, |f| target.serial < f.serial) {
        "rollback"
    } else {
        "activate"
    };

    let facts = paths
        .facts()
        .map_err(|e| format!("device facts unavailable: {e}"))?;
    preflight(&manifest, &facts)?;
    if kind == "activate" {
        if let Some(f) = from.as_ref() {
            // Re-activating the release that is already active is a legitimate
            // repair action; an older serial under a different id is not.
            if target.id != f.id && target.serial <= f.serial {
                return Err(format!(
                    "release serial {} is not newer than the active {}; use rollback for an older release",
                    target.serial, f.serial
                ));
            }
        }
    }
    let elf_check = check_elf(&dir, &manifest);
    if let Err(e) = elf_check {
        return Err(format!("release {target_id}: {e}"));
    }

    // Snapshot before anything changes. A failure here aborts with zero writes.
    let db = snapshot(paths, &target_id)?;
    device::log_line(
        &paths.log_path(),
        &format!(
            "{kind}: {target_id} ({}), snapshot {:?} schema {:?}{}",
            reason,
            db.snapshot,
            db.schema_before,
            if from.is_some() {
                format!(" from {}", from.as_ref().unwrap().id)
            } else {
                " (bootstrap)".into()
            }
        ),
    );

    st.last = Some(LastRun {
        kind: kind.into(),
        status: "running".into(),
        target: Some(target.clone()),
        from: from.clone(),
        to: None,
        started_ms: now,
        criteria: Vec::new(),
        error: None,
        db: Some(db),
        bootstrap: None,
        pid: std::process::id(),
        ended_ms: 0,
    });
    st.save(&paths.state_path())?;

    let old = capture_pointers(paths);
    let mut boot = BootstrapWork::default();
    if let Err(e) = bootstrap(paths, &dir, &manifest, &mut boot) {
        return fail_before_layers(paths, &mut st, &old, &boot, e);
    }
    let kws_backup = match capture_kws_backup(paths, &manifest) {
        Ok(b) => b,
        Err(e) => return fail_before_layers(paths, &mut st, &old, &boot, e),
    };
    if let Some(last) = st.last.as_mut() {
        last.bootstrap = Some(boot.clone());
    }
    st.save(&paths.state_path())?;

    match activate_layers(paths, &dir, &manifest) {
        Ok(kws_work) => {
            // The keyword sidecar cannot run without `kw.txt`, and the seed
            // deliberately ships none (the word list is the node's own
            // configuration). The health gate below requires `kws.state =
            // running`, so on a node that has never had a word list the FIRST
            // `install --activate` could never pass: measured on dot2-b
            // 2026-11-15 — `sidecar exited (exit status: 2)`, `state: "down"`,
            // 90 s of waiting, then a rollback that left the node on the seed.
            // Pushing the node's (default) list here is what makes the
            // activation flow bring the node to a USABLE state instead of
            // requiring a hand-run `POST /reflex/deploy` first.
            let words = ensure_reflex_words(paths);
            let want = Expect::from_manifest(&manifest);
            let (report, ms) = health::wait_healthy(paths, &want, health_budget());
            if report.ok {
                kws_backup.cleanup();
                device::log_line(
                    &paths.log_path(),
                    &format!("{kind}: {target_id} healthy after {ms} ms"),
                );
                target.activated_ms = state::now_ms();
                let mut run = st.last.take().unwrap_or_default();
                run.status = "ok".into();
                run.ended_ms = state::now_ms();
                run.criteria = report.checks;
                // Applied-state feedback for the spotter bundle: "the release
                // was activated" and "its driver is the one the speaker runs"
                // are two different facts (2026-11-15).
                run.criteria.push(state::Check {
                    name: "kws_bundle".into(),
                    ok: true,
                    detail: kws_work.detail.clone(),
                });
                run.to = Some(state::release_ref(&manifest, target.installed_ms));
                // The schema the healthy core is actually running, so the
                // receipt says "5 → 5" instead of leaving the second column
                // blank on a successful activation.
                if let Some(db) = run.db.as_mut() {
                    db.schema_after = paths.status().ok().and_then(|s| {
                        s.get("db")
                            .and_then(|d| d.get("schema"))
                            .and_then(|v| v.as_i64())
                    });
                }
                // Re-activating the release that is already active is a repair action; it
                // must not turn `previous` into a self-reference (that would make
                // "roll back" a no-op and hide the real previous release).
                let same_release = from.as_ref().map(|f| f.id == target.id).unwrap_or(false);
                st.active = Some(target);
                if !same_release {
                    st.previous = from;
                }
                st.record(run);
                st.save(&paths.state_path())?;
                match &words {
                    WordsWork::Deployed { hash, words } => device::log_line(
                        &paths.log_path(),
                        &format!(
                            "activate: reflex words pushed before the health gate (hash {hash}, {words} words)"
                        ),
                    ),
                    WordsWork::AlreadyRunning { hash } => device::log_line(
                        &paths.log_path(),
                        &format!("activate: spotter was already running ({hash}); no word push needed"),
                    ),
                    WordsWork::Unavailable { why } => device::log_line(
                        &paths.log_path(),
                        &format!("activate: word list not pushed: {why}"),
                    ),
                }
                prune(paths, &st);
                Ok(0)
            } else {
                device::log_line(
                    &paths.log_path(),
                    &format!("{kind}: health failed after {ms} ms — rolling back"),
                );
                do_rollback(
                    paths,
                    &mut st,
                    &old,
                    &boot,
                    &kws_backup,
                    from,
                    Some(&report),
                    "health confirmation failed",
                )
            }
        }
        Err(e) => {
            device::log_line(
                &paths.log_path(),
                &format!("{kind}: layer activation failed: {e} — rolling back"),
            );
            do_rollback(paths, &mut st, &old, &boot, &kws_backup, from, None, &e)
        }
    }
}

fn preflight(m: &Manifest, facts: &device::DeviceFacts) -> Result<(), String> {
    if m.target.model != facts.model {
        return Err(format!(
            "model mismatch: release targets {:?}, device reports {:?}",
            m.target.model, facts.model
        ));
    }
    if m.target.profile != facts.profile {
        return Err(format!(
            "profile mismatch: release targets {:?}, device is {:?}",
            m.target.profile, facts.profile
        ));
    }
    if !crate::manifest::version_in_range(&facts.firmware_version, &m.firmware_range) {
        return Err(format!(
            "installed firmware {} is outside the release's supported range [{}, {}]",
            facts.firmware_version, m.firmware_range.min, m.firmware_range.max
        ));
    }
    if !crate::manifest::version_in_range(&m.components.firmware.version, &m.firmware_range) {
        return Err(format!(
            "release firmware {} is outside its own declared range",
            m.components.firmware.version
        ));
    }
    Ok(())
}

fn check_elf(dir: &Path, m: &Manifest) -> Result<(), String> {
    let core =
        std::fs::read(dir.join(&m.components.core.file)).map_err(|e| format!("core: {e}"))?;
    validate_elf(&core, 183, 1024 * 1024)?;
    let fw = std::fs::read(dir.join(&m.components.firmware.file))
        .map_err(|e| format!("firmware: {e}"))?;
    validate_elf(&fw, 40, 100 * 1024)?;
    Ok(())
}

fn snapshot(paths: &Paths, tag: &str) -> Result<DbWork, String> {
    let body = serde_json::json!({ "tag": tag });
    let r = paths.core_post_json("/api/v0/release/snapshot", &body, Duration::from_secs(60))?;
    if r.status != 200 {
        return Err(format!(
            "DB snapshot failed: HTTP {} {}",
            r.status,
            String::from_utf8_lossy(&r.body)
        ));
    }
    let v: Value =
        serde_json::from_slice(&r.body).map_err(|e| format!("snapshot response: {e}"))?;
    let path = v
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or("snapshot response has no path")?
        .to_string();
    let schema = v.get("schema").and_then(|s| s.as_i64());
    // The core's own clock, taken at VACUUM time. The boot guard compares it
    // with the schema sidecar's `ms` instead of file mtimes, because SQLite's
    // writes land in the `-wal` file and leave the DB's mtime behind (M8b).
    let snapshot_ms = v.get("ms").and_then(|m| m.as_u64());
    if !Path::new(&path).is_file() {
        return Err(format!("snapshot reported {path} but the file is missing"));
    }
    Ok(DbWork {
        snapshot: Some(path),
        snapshot_ms,
        schema_before: schema,
        schema_after: None,
        restored: false,
        note: String::new(),
    })
}

#[derive(Debug, Default)]
struct OldPointers {
    core_slot: Option<String>,
    audio_slot: Option<String>,
    release_current: Option<PathBuf>,
    web_link: Option<PathBuf>,
    model_link: Option<PathBuf>,
}

fn capture_pointers(paths: &Paths) -> OldPointers {
    OldPointers {
        core_slot: read_current(&paths.core_bin_dir()),
        audio_slot: read_current(&paths.fw_root),
        release_current: std::fs::read_link(paths.releases().join("current")).ok(),
        web_link: std::fs::read_link(paths.web()).ok(),
        model_link: std::fs::read_link(paths.model_path()).ok(),
    }
}

fn is_real_dir_or_file(p: &Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Ok(md) => !md.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// First activation on an existing deployment: move the real web dir / model
/// file aside and replace them with links into the release. The originals stay
/// available for a rollback.
fn bootstrap(paths: &Paths, dir: &Path, m: &Manifest, b: &mut BootstrapWork) -> Result<(), String> {
    let aside_root = paths.releases().join(".bootstrap");
    std::fs::create_dir_all(&aside_root)
        .map_err(|e| format!("mkdir {}: {e}", aside_root.display()))?;
    let stamp = state::now_ms();

    let web = paths.web();
    if is_real_dir_or_file(&web) {
        let aside = aside_root.join(format!("web-{stamp}"));
        std::fs::rename(&web, &aside).map_err(|e| format!("move web aside: {e}"))?;
        b.web_aside = Some(aside.display().to_string());
    }
    let model = paths.model_path();
    if is_real_dir_or_file(&model) {
        if let Some(parent) = model.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let aside = aside_root.join(format!("model-{stamp}"));
        std::fs::rename(&model, &aside).map_err(|e| format!("move model aside: {e}"))?;
        b.model_aside = Some(aside.display().to_string());
    }

    let web_target = dir.join(m.components.console.dir.trim_end_matches('/'));
    flip_symlink(&web, &web_target)?;
    flip_symlink(&model, &dir.join(&m.components.model.file))?;
    Ok(())
}

/// What happened to the spotter runtime dir during this activation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KwsWork {
    /// `None` = the target release carries no bundle (nothing was touched).
    pub present: bool,
    pub files: usize,
    pub linked: usize,
    pub copied: usize,
    pub detail: String,
}

/// Link the release's spotter bundle into the runtime dir the firmware spawns
/// (`/data/local/tmp/hm-kws`).
///
/// Why hard links and not a symlinked directory: the same dir holds `kw.txt`,
/// which is NODE configuration written by the gateway on every `kws_deploy`.
/// A symlinked dir would put a household's word list inside a signed release
/// directory. Per-file links keep the node's state where it is and make the
/// swap atomic one file at a time (the running spotter keeps its old inodes;
/// the firmware restart that follows re-execs `run.sh` and picks up the new
/// ones).
///
/// A release without the component leaves the bundle exactly as it was: that
/// is the pre-2026-11-15 shape, and a rollback to such a release must not
/// silently "downgrade" a driver nobody has a copy of.
fn sync_kws_runtime(paths: &Paths, dir: &Path, m: &Manifest) -> Result<KwsWork, String> {
    let Some(kws) = &m.components.kws else {
        return Ok(KwsWork {
            present: false,
            detail: "this release carries no spotter bundle; the node's copy was left as is".into(),
            ..Default::default()
        });
    };
    let src_root = dir.join(kws.dir.trim_end_matches('/'));
    let dst_root = paths.kws_dir();
    std::fs::create_dir_all(&dst_root).map_err(|e| format!("mkdir {}: {e}", dst_root.display()))?;
    let mut work = KwsWork {
        present: true,
        ..Default::default()
    };
    for f in &kws.files {
        let src = src_root.join(&f.path);
        let dst = dst_root.join(&f.path);
        if !src.is_file() {
            return Err(format!(
                "release is missing {} — the bundle cannot be installed from a partial dir",
                src.display()
            ));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        // Already this exact inode (an earlier activation of the same release):
        // nothing to do.
        if same_inode(&src, &dst) {
            work.files += 1;
            work.linked += 1;
            continue;
        }
        let tmp = dst
            .parent()
            .map(|p| {
                p.join(format!(
                    ".{}.hm-new",
                    f.path.rsplit('/').next().unwrap_or("file")
                ))
            })
            .unwrap_or_else(|| dst.with_extension("hm-new"));
        let _ = std::fs::remove_file(&tmp);
        match std::fs::hard_link(&src, &tmp) {
            Ok(()) => {
                std::fs::rename(&tmp, &dst)
                    .map_err(|e| format!("rename {}: {e}", dst.display()))?;
                work.linked += 1;
            }
            Err(_) => {
                // Different filesystem (host tests, an unusual layout): copy
                // into place instead of failing the activation.
                std::fs::copy(&src, &tmp).map_err(|e| format!("copy {}: {e}", src.display()))?;
                std::fs::rename(&tmp, &dst)
                    .map_err(|e| format!("rename {}: {e}", dst.display()))?;
                work.copied += 1;
            }
        }
        // Heal a bundle installed by an older packer that wrote 0644: the
        // loader and `run.sh` MUST be executable or the sidecar dies on its
        // first exec, and the gateway then reports "sidecar not installed"
        // (measured dot2-c 2026-11-15). Mode is not hashed, so this cannot
        // invalidate the release.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = crate::manifest::kws_file_mode(&f.path, &kws.entry);
            let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(mode));
        }
        work.files += 1;
    }
    work.detail = format!(
        "{} file(s) linked into {} ({} hard link(s), {} copied); kw.txt untouched",
        work.files,
        dst_root.display(),
        work.linked,
        work.copied
    );
    Ok(work)
}

fn same_inode(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Pre-activation snapshot of files in `paths.kws_dir()` that the incoming
/// release will touch, so a rollback can restore pre-existing files or remove
/// newly introduced files even when rolling back to a pre-KWS release.
#[derive(Debug, Default)]
struct KwsBackup {
    backup_dir: Option<PathBuf>,
    backed_up_files: Vec<String>,
    added_files: Vec<String>,
}

impl KwsBackup {
    fn cleanup(&self) {
        if let Some(dir) = &self.backup_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn capture_kws_backup(paths: &Paths, m: &Manifest) -> Result<KwsBackup, String> {
    let Some(kws) = &m.components.kws else {
        return Ok(KwsBackup::default());
    };
    let dst_root = paths.kws_dir();
    let mut backup = KwsBackup::default();
    for f in &kws.files {
        if f.path == "kw.txt" {
            continue;
        }
        let dst = dst_root.join(&f.path);
        if dst.is_file() {
            let bdir = match &backup.backup_dir {
                Some(d) => d.clone(),
                None => {
                    let d = paths
                        .releases()
                        .join(".bootstrap")
                        .join(format!("kws-{}", state::now_ms()));
                    std::fs::create_dir_all(&d)
                        .map_err(|e| format!("mkdir {}: {e}", d.display()))?;
                    backup.backup_dir = Some(d.clone());
                    d
                }
            };
            let saved = bdir.join(&f.path);
            if let Some(parent) = saved.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    backup.cleanup();
                    return Err(format!("mkdir {}: {e}", parent.display()));
                }
            }
            if std::fs::hard_link(&dst, &saved).is_err() {
                if let Err(e) = std::fs::copy(&dst, &saved) {
                    backup.cleanup();
                    return Err(format!("backup kws {}: {e}", dst.display()));
                }
            }
            backup.backed_up_files.push(f.path.clone());
        } else {
            backup.added_files.push(f.path.clone());
        }
    }
    Ok(backup)
}

fn restore_kws_on_rollback(
    paths: &Paths,
    from: Option<&ReleaseRef>,
    backup: &KwsBackup,
) -> Result<Option<String>, String> {
    let dst_root = paths.kws_dir();
    if let Some(prev_ref) = from {
        let prev_dir = paths.releases().join(&prev_ref.id);
        let manifest_path = prev_dir.join("manifest.json");
        if manifest_path.is_file() {
            if let Ok(raw) = std::fs::read(&manifest_path) {
                if let Ok(prev_manifest) = Manifest::from_bytes(&raw) {
                    if let Some(prev_kws) = &prev_manifest.components.kws {
                        let prev_paths: std::collections::HashSet<&str> =
                            prev_kws.files.iter().map(|f| f.path.as_str()).collect();
                        for added in &backup.added_files {
                            if added != "kw.txt" && !prev_paths.contains(added.as_str()) {
                                let _ = std::fs::remove_file(dst_root.join(added));
                            }
                        }
                        if let Ok(work) = sync_kws_runtime(paths, &prev_dir, &prev_manifest) {
                            backup.cleanup();
                            return Ok(Some(format!(
                                "restored from {}: {}",
                                prev_ref.id, work.detail
                            )));
                        }
                    }
                }
            }
        }
    }

    if backup.backed_up_files.is_empty() && backup.added_files.is_empty() {
        backup.cleanup();
        return Ok(None);
    }
    if let Some(bdir) = &backup.backup_dir {
        for rel in &backup.backed_up_files {
            if rel == "kw.txt" {
                continue;
            }
            let src = bdir.join(rel);
            let dst = dst_root.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            let tmp = dst
                .parent()
                .map(|p| {
                    p.join(format!(
                        ".{}.hm-rb",
                        rel.rsplit('/').next().unwrap_or("file")
                    ))
                })
                .unwrap_or_else(|| dst.with_extension("hm-rb"));
            let _ = std::fs::remove_file(&tmp);
            if std::fs::hard_link(&src, &tmp).is_err() {
                std::fs::copy(&src, &tmp)
                    .map_err(|e| format!("restore kws {}: {e}", dst.display()))?;
            }
            std::fs::rename(&tmp, &dst).map_err(|e| format!("rename {}: {e}", dst.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = crate::manifest::kws_file_mode(rel, "run.sh");
                let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    for rel in &backup.added_files {
        if rel == "kw.txt" {
            continue;
        }
        let _ = std::fs::remove_file(dst_root.join(rel));
    }
    let detail = format!(
        "restored {} pre-existing file(s), removed {} new file(s); kw.txt untouched",
        backup.backed_up_files.len(),
        backup.added_files.len()
    );
    backup.cleanup();
    Ok(Some(detail))
}

fn activate_layers(paths: &Paths, dir: &Path, m: &Manifest) -> Result<KwsWork, String> {
    let core_dir = paths.core_bin_dir();
    let audio_dir = paths.fw_root.clone();

    let core_slot = inactive_slot(&core_dir, CORE_SLOTS).ok_or_else(|| {
        format!(
            "no core slot to write (missing {} /current)",
            core_dir.display()
        )
    })?;
    let audio_slot = inactive_slot(&audio_dir, AUDIO_SLOTS).ok_or_else(|| {
        format!(
            "no audio slot to write (missing {}/current)",
            audio_dir.display()
        )
    })?;

    write_slot_from_file(&core_dir, &core_slot, &dir.join(&m.components.core.file))?;
    write_slot_from_file(
        &audio_dir,
        &audio_slot,
        &dir.join(&m.components.firmware.file),
    )?;
    device::log_line(
        &paths.log_path(),
        &format!("activate: slots written ({core_slot}, {audio_slot})"),
    );

    flip_symlink(&core_dir.join("current"), Path::new(&core_slot))?;
    flip_symlink(&audio_dir.join("current"), Path::new(&audio_slot))?;
    flip_symlink(&paths.releases().join("current"), dir)?;
    // Spotter bundle BEFORE the firmware restart below: that restart is what
    // re-execs run.sh, so the links must already point at this release.
    let kws_work = sync_kws_runtime(paths, dir, m)?;
    device::log_line(
        &paths.log_path(),
        &format!("activate: kws {}", kws_work.detail),
    );
    device::log_line(
        &paths.log_path(),
        "activate: pointers flipped — restarting firmware then core",
    );

    // Firmware first: its supervisor exits on code 0 and init respawns it from
    // the flipped slot. Wait for the PROCESS to change (a version string alone
    // could match the old binary, e.g. a rebuild of the same version) and then
    // for the device to report the target version.
    let old_fw = paths.firmware_pids();
    if !paths.restart_firmware()? {
        device::log_line(
            &paths.log_path(),
            "activate: no firmware process found (host simulation?); health decides",
        );
    }
    if !old_fw.is_empty() {
        let replaced = wait_until(firmware_budget(), Duration::from_millis(300), || {
            let now = paths.firmware_pids();
            !now.is_empty() && now.iter().all(|p| !old_fw.contains(p))
        });
        if !replaced {
            return Err(format!(
                "firmware process did not restart (old pids {old_fw:?}) within {} s",
                firmware_budget().as_secs()
            ));
        }
        device::log_line(
            &paths.log_path(),
            &format!("activate: firmware process replaced (was {old_fw:?})"),
        );
    }
    let want_fw = m.components.firmware.version.clone();
    let fw_ok = wait_until(firmware_budget(), Duration::from_millis(500), || {
        paths
            .facts()
            .map(|f| f.firmware_version == want_fw)
            .unwrap_or(false)
    });
    if !fw_ok {
        return Err(format!(
            "firmware did not come back as {want_fw} within {} s",
            firmware_budget().as_secs()
        ));
    }
    device::log_line(
        &paths.log_path(),
        &format!("activate: firmware {want_fw} is up"),
    );

    let old_core = paths.core_pids();
    paths.restart_core()?;
    if !old_core.is_empty() {
        let replaced = wait_until(core_budget(), Duration::from_millis(300), || {
            let now = paths.core_pids();
            !now.is_empty() && now.iter().all(|p| !old_core.contains(p))
        });
        if !replaced {
            return Err(format!(
                "core process did not restart (old pids {old_core:?}) within {} s",
                core_budget().as_secs()
            ));
        }
        device::log_line(
            &paths.log_path(),
            &format!("activate: core process replaced (was {old_core:?})"),
        );
    }
    Ok(kws_work)
}

#[allow(clippy::too_many_arguments)]
fn do_rollback(
    paths: &Paths,
    st: &mut State,
    old: &OldPointers,
    boot: &BootstrapWork,
    kws_backup: &KwsBackup,
    from: Option<ReleaseRef>,
    report: Option<&Report>,
    error: &str,
) -> Result<i32, String> {
    let mut run = st.last.take().unwrap_or_default();
    run.status = "rolling_back".into();
    if let Some(r) = report {
        run.criteria = r.checks.clone();
    }
    run.error = Some(error.to_string());
    // Intermediate state only: history keeps finished runs (no ended_ms=0).
    st.last = Some(run.clone());
    st.save(&paths.state_path())?;
    device::log_line(&paths.log_path(), &format!("rollback: {error}"));

    restore_pointers(paths, old, boot)?;
    let mut checks = Vec::new();
    let mut kws_restore_ok = true;
    match restore_kws_on_rollback(paths, from.as_ref(), kws_backup) {
        Ok(Some(detail)) => {
            device::log_line(&paths.log_path(), &format!("rollback: kws {detail}"));
            checks.push(Check {
                name: "rollback_kws".into(),
                ok: true,
                detail,
            });
        }
        Ok(None) => {}
        Err(e) => {
            kws_restore_ok = false;
            device::log_line(
                &paths.log_path(),
                &format!("rollback: kws restore failed: {e}"),
            );
            checks.push(Check {
                name: "rollback_kws".into(),
                ok: false,
                detail: e,
            });
        }
    }

    // DB: restore the snapshot only when a schema migration advanced past the
    // one the snapshot was taken at. Otherwise the window's writes are kept.
    let sidecar = paths.schema_sidecar_value();
    run.db = run.db.or_else(|| Some(DbWork::default()));
    if let Some(db) = run.db.as_mut() {
        db.schema_after = sidecar;
        let advanced = matches!(
            (sidecar, db.schema_before),
            (Some(after), Some(before)) if after > before
        );
        if advanced {
            let marker = serde_json::json!({
                "snapshot": db.snapshot,
                "schema": sidecar,
                "for_build": from.as_ref().map(|f| f.build_id.clone()).unwrap_or_default(),
                "why": format!("rollback after {error}"),
            });
            state::write_atomic(
                &paths.releases().join("restore-on-boot.json"),
                &serde_json::to_vec_pretty(&marker).unwrap_or_default(),
            )?;
            db.note = "schema advanced during activation; the core restores the snapshot before opening the DB".into();
            db.restored = true;
            device::log_line(
                &paths.log_path(),
                "rollback: DB restore scheduled (boot guard)",
            );
        } else {
            db.note = "schema unchanged; current DB kept".into();
        }
    }
    st.save(&paths.state_path())?;

    // The firmware may have been restarted into the new (bad) slot; put it
    // back too. Best effort: health below decides whether the old release is
    // really serving again.
    if let Ok(signalled) = paths.restart_firmware() {
        if !signalled {
            device::log_line(
                &paths.log_path(),
                "rollback: no firmware process to restart",
            );
        }
    }
    let _ = paths.restart_core();
    let healthy = if let Some(f) = from.as_ref().filter(|r| !r.build_id.is_empty()) {
        let want = Expect::from_ref(f);
        let (rep, _) = health::wait_healthy(paths, &want, rollback_budget());
        for c in rep.checks {
            checks.push(Check {
                name: format!("rollback_{}", c.name),
                ..c
            });
        }
        rep.ok
    } else {
        let alive = wait_until(rollback_budget(), Duration::from_millis(500), || {
            paths.status().is_ok()
        });
        checks.push(Check {
            name: "rollback_core".into(),
            ok: alive,
            detail: "previous slot reachable (no previous release was recorded)".into(),
        });
        alive
    };
    let healthy = healthy && kws_restore_ok;

    let mut run = st.last.take().unwrap_or_default();
    run.criteria.extend(checks);
    run.ended_ms = state::now_ms();
    run.status = if healthy {
        "rolled_back".into()
    } else {
        "rollback_failed".into()
    };
    // What is serving now: the previous release when the rollback worked;
    // NULL when there was nothing to go back to.
    run.to = if healthy { from.clone() } else { None };
    device::log_line(
        &paths.log_path(),
        &format!("rollback: final status {}", run.status),
    );
    st.record(run);
    st.save(&paths.state_path())?;
    Ok(if healthy { 1 } else { 2 })
}

fn restore_pointers(paths: &Paths, old: &OldPointers, boot: &BootstrapWork) -> Result<(), String> {
    let core_current = paths.core_bin_dir().join("current");
    if let Some(slot) = &old.core_slot {
        flip_symlink(&core_current, Path::new(slot))?;
    }
    if let Some(slot) = &old.audio_slot {
        flip_symlink(&paths.fw_root.join("current"), Path::new(slot))?;
    }
    let release_current = paths.releases().join("current");
    match &old.release_current {
        Some(target) => flip_symlink(&release_current, target)?,
        None => {
            let _ = std::fs::remove_file(&release_current);
        }
    }

    // Web: an adopted real directory goes back verbatim; otherwise the old
    // symlink target is restored (or the link is removed if there was none).
    let web = paths.web();
    if let Some(aside) = &boot.web_aside {
        let _ = std::fs::remove_file(&web);
        std::fs::rename(aside, &web).map_err(|e| format!("restore web dir: {e}"))?;
    } else if let Some(target) = &old.web_link {
        flip_symlink(&web, target)?;
    } else {
        let _ = std::fs::remove_file(&web);
    }

    let model = paths.model_path();
    if let Some(aside) = &boot.model_aside {
        let _ = std::fs::remove_file(&model);
        std::fs::rename(aside, &model).map_err(|e| format!("restore model file: {e}"))?;
    } else if let Some(target) = &old.model_link {
        flip_symlink(&model, target)?;
    } else {
        let _ = std::fs::remove_file(&model);
    }
    Ok(())
}

/// A failure before any layer moved: restore the adopted pointers, no restart
/// is needed because the running core still uses the old files.
fn fail_before_layers(
    paths: &Paths,
    st: &mut State,
    old: &OldPointers,
    boot: &BootstrapWork,
    error: String,
) -> Result<i32, String> {
    let _ = restore_pointers(paths, old, boot);
    let mut run = st.last.take().unwrap_or_default();
    run.status = "failed".into();
    run.error = Some(error.clone());
    run.ended_ms = state::now_ms();
    run.to = st.active.clone();
    st.record(run);
    st.save(&paths.state_path())?;
    device::log_line(
        &paths.log_path(),
        &format!("activate failed before layers: {error}"),
    );
    Err(error)
}

/// What happened to the node's word list before the health gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordsWork {
    /// A push was needed and the core accepted it.
    Deployed { hash: String, words: u64 },
    /// The spotter is already running with a list; nothing was sent, so a
    /// healthy node is not disturbed by a needless sidecar restart (~7 s).
    AlreadyRunning { hash: String },
    /// No core answered, or the push was refused. Never fatal here: the
    /// health gate below reports the consequence with its own evidence.
    Unavailable { why: String },
}

/// Should activation push the word list? (R1j + contract v2 #5)
///
/// - no running spotter → always (a fresh node has no `kw.txt`);
/// - running spotter with a local exit armed → leave it alone (a needless
///   restart is felt as a 7 s deaf spell);
/// - running spotter whose table HAS a retreat word but the device armed
///   NONE → push: the vocabulary is loaded, its MEANING is not.
///
/// `local_exit` is the DEVICE's own count (`/mode.kws.local_exit`), not this
/// core's deploy record: that record lives in memory, so after every core
/// restart it reads 0 and would re-push (and restart the spotter) forever.
fn needs_reflex_push(kws_state: &str, wants_exit: bool, local_exit: u32) -> bool {
    if kws_state != "running" {
        return true;
    }
    wants_exit && local_exit == 0
}

/// Make sure the node HAS a word list before the health gate asks for
/// `kws.state = running`.
///
/// A fresh node has none (the seed ships no `kw.txt` by design) and a spotter
/// without one exits, so this is the difference between "the activation brings
/// the node up" and "the operator must know to call /reflex/deploy first".
/// A node whose spotter is already running is left alone — UNLESS its local
/// exit is not armed (contract v2 §2.5 #5): "the vocabulary is loaded" and
/// "its retreat meaning is on this device" are two facts, and only a deploy
/// installs the second. Arming it costs one spotter restart (~7 s) and is the
/// difference between a 300 ms local exit and a silent no-op.
fn ensure_reflex_words(paths: &Paths) -> WordsWork {
    let mode = match paths.core_request("GET", "/api/v0/mode", None, Duration::from_secs(5)) {
        Ok(r) if r.status == 200 => serde_json::from_slice::<Value>(&r.body).ok(),
        Ok(r) => {
            return WordsWork::Unavailable {
                why: format!("GET /mode answered HTTP {}", r.status),
            }
        }
        Err(e) => {
            return WordsWork::Unavailable {
                why: format!("core did not answer /mode: {e}"),
            }
        }
    };
    let device = mode
        .as_ref()
        .and_then(|m| m.get("devices"))
        .and_then(|d| d.as_array())
        .and_then(|d| d.first());
    let kws_state = device
        .and_then(|d| d.get("kws"))
        .and_then(|k| k.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let loaded = device
        .and_then(|d| d.get("reflex"))
        .and_then(|r| r.get("deploy"))
        .and_then(|d| d.get("loaded_hash"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Contract v2 §2.5 (#5): does the SAVED table contain a retreat word, and
    // has the DEVICE armed one? `/mode` reports both; a missing key reads as
    // "not armed" (an older core: nothing to arm).
    let wants_exit = device
        .and_then(|d| d.get("reflex"))
        .and_then(|r| r.get("words"))
        .and_then(|w| w.as_array())
        .map(|words| {
            words.iter().any(|w| {
                w.get("intent").and_then(|i| i.as_str()) == Some("retreat")
                    && w.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false)
            })
        })
        .unwrap_or(false);
    // The DEVICE's armed count (durable). An older gateway reports nothing:
    // fall back to this core's deploy receipt so a fresh deploy still counts
    // as armed instead of pushing on every activation.
    let deploy = device
        .and_then(|d| d.get("reflex"))
        .and_then(|r| r.get("deploy"));
    let local_exit = device
        .and_then(|d| d.get("kws"))
        .and_then(|k| k.get("local_exit"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .or_else(|| {
            deploy
                .and_then(|d| d.get("local_exit"))
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
        })
        .unwrap_or(0);
    if !needs_reflex_push(kws_state, wants_exit, local_exit) {
        return WordsWork::AlreadyRunning {
            hash: loaded.to_string(),
        };
    }
    match paths.core_request(
        "POST",
        "/api/v0/reflex/deploy",
        None,
        Duration::from_secs(15),
    ) {
        Ok(r) if r.status == 200 => {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            WordsWork::Deployed {
                hash: v
                    .get("hash")
                    .and_then(|h| h.as_str())
                    .unwrap_or("")
                    .to_string(),
                words: v.get("words").and_then(|w| w.as_u64()).unwrap_or(0),
            }
        }
        Ok(r) => WordsWork::Unavailable {
            why: format!("POST /reflex/deploy answered HTTP {}", r.status),
        },
        Err(e) => WordsWork::Unavailable {
            why: format!("POST /reflex/deploy failed: {e}"),
        },
    }
}

/// Keep exactly the active release and the previous one. On this device /data
/// is ~1 GB shared with everything else; a museum of old builds is what made
/// the next install fail on free space. DB snapshots are pruned by the core.
fn prune(paths: &Paths, st: &State) {
    // active + previous + a just-installed (not yet activated) target.
    let keep = st.keep_ids();
    let Ok(dir) = std::fs::read_dir(paths.releases()) else {
        return;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `current` is a symlink to the active release: never a release dir.
        // Following it here once deleted the activation pointer.
        if name.starts_with('.') || name == "current" || keep.contains(&name) {
            continue;
        }
        match std::fs::symlink_metadata(entry.path()) {
            Ok(m) if m.file_type().is_dir() && !m.file_type().is_symlink() => {}
            _ => continue,
        }
        let _ = std::fs::remove_dir_all(entry.path());
        device::log_line(
            &paths.log_path(),
            &format!("prune: removed old release {name}"),
        );
    }
}

/// Clear `previous` when it points at the active release. Returns whether the
/// state changed.
fn repair_previous(st: &mut State) -> bool {
    let same = match (&st.active, &st.previous) {
        (Some(a), Some(p)) => a.id == p.id,
        _ => false,
    };
    if same {
        st.previous = None;
    }
    same
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ReleaseRef, State};

    fn rel(id: &str) -> ReleaseRef {
        ReleaseRef {
            id: id.into(),
            serial: id.split('-').next().unwrap().parse().unwrap(),
            ..Default::default()
        }
    }

    /// Contract v2 §2.5 (#5): activation pushes the word list when the node
    /// has no spotter — and ALSO when the spotter runs on a vocabulary whose
    /// retreat meaning was never armed. It must not restart a healthy spotter
    /// whose local exit is already in force (a needless 7 s deaf spell).
    #[test]
    fn activation_pushes_the_word_list_exactly_when_it_can_help() {
        // No running spotter (fresh node): always push.
        assert!(needs_reflex_push("off", true, 0));
        assert!(needs_reflex_push("", false, 0));
        assert!(needs_reflex_push("down", true, 3));
        // Running spotter, no retreat word in the table: leave it alone.
        assert!(!needs_reflex_push("running", false, 0));
        // Running spotter, retreat word saved, NOTHING armed: the vocabulary
        // is loaded but its meaning is not — push once to arm it.
        assert!(needs_reflex_push("running", true, 0));
        // Armed: the local exit is in force. Never push again, or every
        // activation after a core restart would cost a 7 s spotter restart.
        assert!(!needs_reflex_push("running", true, 1));
        assert!(!needs_reflex_push("running", true, 2));
    }

    #[test]
    fn prune_keeps_current_previous_and_the_pointer() {
        let root = std::env::temp_dir().join(format!("hm-rel-aprune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::for_test(root.join("hm"), root.join("fw"));
        paths.ensure_dirs().unwrap();
        for name in ["1-a", "2-b", "3-c"] {
            std::fs::create_dir_all(paths.releases().join(name)).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            paths.releases().join("2-b"),
            paths.releases().join("current"),
        )
        .unwrap();
        let st = State {
            active: Some(rel("2-b")),
            previous: Some(rel("1-a")),
            ..Default::default()
        };
        prune(&paths, &st);
        assert!(paths.releases().join("1-a").is_dir());
        assert!(paths.releases().join("2-b").is_dir());
        assert!(!paths.releases().join("3-c").exists());
        assert!(paths.releases().join("current").is_symlink());
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod kws_tests {
    use super::*;
    use crate::manifest::{
        Components, ConsoleComponent, CoreComponent, FirmwareComponent, FirmwareRange,
        KwsComponent, KwsFile, Manifest, ModelComponent, ReleaseInfo, Target,
    };

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hm-kws-sync-{tag}-{}-{}",
            std::process::id(),
            state::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn manifest_with_bundle(dir: &Path, with_kws: bool) -> Manifest {
        let kws = if with_kws {
            let root = dir.join("components/kws");
            let mut files = Vec::new();
            for rel in ["hm-kws", "run.sh", "model/tokens.txt"] {
                let p = root.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, format!("bytes of {rel}")).unwrap();
                files.push(KwsFile {
                    path: rel.into(),
                    size: std::fs::metadata(&p).unwrap().len(),
                    sha256: crate::crypto::sha256_file(&p).unwrap(),
                });
            }
            Some(KwsComponent {
                dir: "components/kws".into(),
                entry: "run.sh".into(),
                files,
            })
        } else {
            None
        };
        Manifest {
            manifest_version: 1,
            release: ReleaseInfo {
                serial: 1,
                version: "t".into(),
                created_ms: 0,
                notes: String::new(),
            },
            target: Target {
                model: "AEOBC".into(),
                profile: "dot2d-emos".into(),
                arch: "aarch64".into(),
            },
            firmware_range: FirmwareRange {
                min: "0.0.44".into(),
                max: "0.0.99".into(),
            },
            components: Components {
                core: CoreComponent {
                    file: "components/core-aarch64".into(),
                    size: 1,
                    sha256: "a".repeat(64),
                    version: "0.0.1".into(),
                    build_id: "testbuild".into(),
                },
                firmware: FirmwareComponent {
                    file: "components/firmware-armv7".into(),
                    size: 1,
                    sha256: "b".repeat(64),
                    version: "0.0.46".into(),
                    gateway_version: "0.0.46".into(),
                    build_id: "fw".into(),
                    roles: vec!["firmware".into(), "gateway".into()],
                },
                console: ConsoleComponent {
                    dir: "components/web".into(),
                    index: "index-x.js".into(),
                    source_commit: "t".into(),
                    files: Vec::new(),
                },
                model: ModelComponent {
                    file: "components/model/silero_vad.onnx".into(),
                    size: 1,
                    sha256: "c".repeat(64),
                    name: "silero_vad".into(),
                },
                kws,
            },
            total_size: 0,
        }
    }

    #[test]
    fn the_runtime_dir_gets_links_and_keeps_the_word_list() {
        let dir = temp("link");
        let release = dir.join("rel");
        let m = manifest_with_bundle(&release, true);
        let paths = Paths::for_test(dir.join("hm"), dir.join("fw"));
        // The node's own state lives in the same dir as the bundle.
        std::fs::create_dir_all(paths.kws_dir()).unwrap();
        std::fs::write(paths.kws_dir().join("kw.txt"), "n ǐ h ǎo @你好\n").unwrap();
        std::fs::write(paths.kws_dir().join("hm-kws"), b"stale driver").unwrap();

        let work = sync_kws_runtime(&paths, &release, &m).unwrap();
        assert!(work.present);
        assert_eq!(work.files, 3);
        assert_eq!(
            std::fs::read(paths.kws_dir().join("hm-kws")).unwrap(),
            b"bytes of hm-kws"
        );
        assert_eq!(
            std::fs::read(paths.kws_dir().join("model/tokens.txt")).unwrap(),
            b"bytes of model/tokens.txt"
        );
        // The word list is node configuration: same content, and the release
        // dir must never hold one.
        assert_eq!(
            std::fs::read_to_string(paths.kws_dir().join("kw.txt")).unwrap(),
            "n ǐ h ǎo @你好\n"
        );
        assert!(!release.join("components/kws/kw.txt").exists());
        // Same filesystem → hard links, so two releases can share 27 MB.
        assert!(
            same_inode(
                &release.join("components/kws/hm-kws"),
                &paths.kws_dir().join("hm-kws")
            ),
            "the runtime copy must be a link to the verified release file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bin_mode = std::fs::metadata(paths.kws_dir().join("hm-kws"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let tok_mode = std::fs::metadata(paths.kws_dir().join("model/tokens.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(bin_mode, 0o755, "hm-kws executable must be 0755");
            assert_eq!(
                tok_mode, 0o644,
                "non-executable model asset must stay 0644 (#238)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_without_the_bundle_leaves_the_node_alone() {
        let dir = temp("absent");
        let release = dir.join("rel");
        std::fs::create_dir_all(&release).unwrap();
        let m = manifest_with_bundle(&release, false);
        let paths = Paths::for_test(dir.join("hm"), dir.join("fw"));
        std::fs::create_dir_all(paths.kws_dir()).unwrap();
        std::fs::write(
            paths.kws_dir().join("hm-kws"),
            b"driver from an older release",
        )
        .unwrap();
        std::fs::write(paths.kws_dir().join("kw.txt"), "keep me").unwrap();

        let work = sync_kws_runtime(&paths, &release, &m).unwrap();
        assert!(!work.present);
        assert!(work.detail.contains("left as is"), "{}", work.detail);
        assert_eq!(
            std::fs::read(paths.kws_dir().join("hm-kws")).unwrap(),
            b"driver from an older release"
        );
        assert_eq!(
            std::fs::read_to_string(paths.kws_dir().join("kw.txt")).unwrap(),
            "keep me"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_bundle_file_fails_instead_of_half_installing() {
        let dir = temp("partial");
        let release = dir.join("rel");
        let m = manifest_with_bundle(&release, true);
        std::fs::remove_file(release.join("components/kws/hm-kws")).unwrap();
        let paths = Paths::for_test(dir.join("hm"), dir.join("fw"));
        let err = sync_kws_runtime(&paths, &release, &m).unwrap_err();
        assert!(err.contains("missing"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_restores_previous_release_kws_and_keeps_kw_txt() {
        let dir = temp("rb-prev");
        let paths = Paths::for_test(dir.join("hm"), dir.join("fw"));
        paths.ensure_dirs().unwrap();

        let prev_dir = paths.releases().join("1-prev");
        let m_prev = manifest_with_bundle(&prev_dir, true);
        std::fs::write(prev_dir.join("components/kws/hm-kws"), b"driver-v1").unwrap();
        std::fs::write(prev_dir.join("manifest.json"), m_prev.to_bytes()).unwrap();

        let next_dir = paths.releases().join("2-next");
        let mut m_next = manifest_with_bundle(&next_dir, true);
        std::fs::write(next_dir.join("components/kws/hm-kws"), b"driver-v2-bad").unwrap();
        let extra_path = next_dir.join("components/kws/extra.so");
        std::fs::write(&extra_path, b"extra-v2").unwrap();
        m_next.components.kws.as_mut().unwrap().files.push(KwsFile {
            path: "extra.so".into(),
            size: 8,
            sha256: crate::crypto::sha256_bytes(b"extra-v2"),
        });

        sync_kws_runtime(&paths, &prev_dir, &m_prev).unwrap();
        std::fs::write(paths.kws_dir().join("kw.txt"), "keep-me\n").unwrap();

        let backup = capture_kws_backup(&paths, &m_next).unwrap();
        sync_kws_runtime(&paths, &next_dir, &m_next).unwrap();
        assert_eq!(
            std::fs::read(paths.kws_dir().join("hm-kws")).unwrap(),
            b"driver-v2-bad"
        );
        assert!(paths.kws_dir().join("extra.so").is_file());

        let prev_ref = ReleaseRef {
            id: "1-prev".into(),
            serial: 1,
            ..Default::default()
        };
        let detail = restore_kws_on_rollback(&paths, Some(&prev_ref), &backup)
            .unwrap()
            .expect("rollback should report kws restoration");
        assert!(detail.contains("1-prev"), "{detail}");
        assert_eq!(
            std::fs::read(paths.kws_dir().join("hm-kws")).unwrap(),
            b"driver-v1"
        );
        assert!(
            !paths.kws_dir().join("extra.so").exists(),
            "newly added kws file must be removed"
        );
        assert_eq!(
            std::fs::read_to_string(paths.kws_dir().join("kw.txt")).unwrap(),
            "keep-me\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_to_pre_kws_release_restores_backup_and_removes_new_files() {
        let dir = temp("rb-backup");
        let paths = Paths::for_test(dir.join("hm"), dir.join("fw"));
        paths.ensure_dirs().unwrap();

        std::fs::create_dir_all(paths.kws_dir()).unwrap();
        std::fs::write(paths.kws_dir().join("hm-kws"), b"preexisting-driver").unwrap();
        std::fs::write(paths.kws_dir().join("kw.txt"), "keep-me\n").unwrap();

        let next_dir = paths.releases().join("2-next");
        let m_next = manifest_with_bundle(&next_dir, true);
        let backup = capture_kws_backup(&paths, &m_next).unwrap();
        sync_kws_runtime(&paths, &next_dir, &m_next).unwrap();
        assert_eq!(
            std::fs::read(paths.kws_dir().join("hm-kws")).unwrap(),
            b"bytes of hm-kws"
        );
        assert!(paths.kws_dir().join("run.sh").is_file());

        let detail = restore_kws_on_rollback(&paths, None, &backup)
            .unwrap()
            .expect("backup restore should report detail");
        assert!(detail.contains("restored 1"), "{detail}");
        assert_eq!(
            std::fs::read(paths.kws_dir().join("hm-kws")).unwrap(),
            b"preexisting-driver"
        );
        assert!(
            !paths.kws_dir().join("run.sh").exists(),
            "newly added file must be removed"
        );
        assert_eq!(
            std::fs::read_to_string(paths.kws_dir().join("kw.txt")).unwrap(),
            "keep-me\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
