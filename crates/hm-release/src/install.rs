//! `install`: fetch → structural/device preflight → hash+extract into staging
//! → Ed25519 → commit. A failure at any step leaves the running release, the
//! slots and every pointer untouched.

use std::path::Path;

use crate::crypto;
use crate::device::{self, validate_elf, FileLock, Paths};
use crate::http;
use crate::manifest::{version_in_range, Manifest};
use crate::package::{self, PackEntry};
use crate::state::{self, LastRun, State};
use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

pub fn run(
    paths: &Paths,
    source: &str,
    activate: bool,
    drill_stall_ms: Option<u64>,
) -> Result<i32, String> {
    install_with_model(
        paths,
        source,
        activate,
        drill_stall_ms,
        crate::manifest::SILERO_VAD_SHA256,
    )
}

/// Test seam: the same install flow against an explicit expected model hash,
/// for HOST integration tests whose fixture packages carry a fake model. It
/// exists only with the `test-seams` feature (enabled by this crate's own
/// dev-dependency), so a production build has no such entry point; `run`
/// always pins the audited `silero_vad.onnx`.
#[cfg(any(test, feature = "test-seams"))]
pub fn run_with_model(
    paths: &Paths,
    source: &str,
    activate: bool,
    drill_stall_ms: Option<u64>,
    expected_model_sha: &str,
) -> Result<i32, String> {
    install_with_model(paths, source, activate, drill_stall_ms, expected_model_sha)
}

fn install_with_model(
    paths: &Paths,
    source: &str,
    activate: bool,
    drill_stall_ms: Option<u64>,
    expected_model_sha: &str,
) -> Result<i32, String> {
    let _lock = FileLock::acquire(&paths.lock_path())?;
    paths.ensure_dirs()?;
    let mut st = State::load(&paths.state_path())?;
    // Free space first, then fetch: /data is ~1 GB shared with everything
    // else. Holding the lock means no other job can own staging/incoming, so
    // leftovers are safe to delete; the previous release is kept for rollback.
    prune_before_install(paths, &st);

    // 1. Fetch (LAN URL or a local path) into releases/.incoming-<pid>.
    let incoming = paths
        .releases()
        .join(format!(".incoming-{}", std::process::id()));
    let _ = std::fs::remove_file(&incoming);
    let size = if source.starts_with("http://") {
        device::log_line(&paths.log_path(), &format!("install: fetching {source}"));
        http::download(source, &incoming, FETCH_TIMEOUT)?
    } else {
        let src = Path::new(source);
        std::fs::copy(src, &incoming).map_err(|e| format!("copy {}: {e}", src.display()))?;
        std::fs::metadata(&incoming).map(|m| m.len()).unwrap_or(0)
    };
    device::log_line(&paths.log_path(), &format!("install: fetched {size} bytes"));

    // 2. Device facts for the preflight (before any component byte is written).
    let facts = paths.facts().map_err(|e| {
        format!("device facts unavailable ({e}); install needs the core to answer /mode")
    })?;
    let active_serial = st.active_serial();

    // 3. Verify + extract into a unique staging directory. The incoming file
    //    already occupies `size`; staging needs about as much again. Refuse
    //    up front with a named error instead of dying mid-write on a full
    //    /data (the device shares ~1 GB with everything else).
    if let Some(free) = device::free_bytes(&paths.releases()) {
        device::log_line(
            &paths.log_path(),
            &format!(
                "install: free {free} bytes, incoming {size} bytes, need {} for staging",
                size.saturating_add(8 * 1024 * 1024)
            ),
        );
        let needed = size.saturating_add(8 * 1024 * 1024);
        if free < needed {
            let _ = std::fs::remove_file(&incoming);
            let msg = format!(
                "not enough free space for staging: {free} bytes free, need about {needed}"
            );
            device::log_line(&paths.log_path(), &format!("install: REJECTED {msg}"));
            return Err(msg);
        }
    }
    let staging = paths.releases().join(format!(
        ".staging-{}-{}",
        std::process::id(),
        state::now_ms()
    ));
    std::fs::create_dir_all(&staging).map_err(|e| format!("mkdir {}: {e}", staging.display()))?;

    let f = match std::fs::File::open(&incoming) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&incoming);
            return Err(format!("open {}: {e}", incoming.display()));
        }
    };
    // Admission (not just format): embedded key + the pinned silero_vad.onnx,
    // then this device's own preflight. The core's own VAD marker is admitted
    // right after extraction (`check_elf_inside`) — it is a fact about the
    // core bytes, which do not exist before then.
    let staged = package::verify_admission(
        std::io::BufReader::new(f),
        Some(&staging),
        |m| preflight(m, &facts, active_serial),
        expected_model_sha,
    );

    let verified = match staged {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            let _ = std::fs::remove_file(&incoming);
            device::log_line(&paths.log_path(), &format!("install: REJECTED {e}"));
            return Err(e);
        }
    };

    // 4. The extracted bytes must really be the architectures the manifest
    //    claims (the tar hash proved the bytes; this proves they can run here).
    if let Err(e) = check_elf_inside(&staging, &verified.manifest) {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&incoming);
        device::log_line(&paths.log_path(), &format!("install: REJECTED {e}"));
        return Err(e);
    }

    // 4b. Files this release shares byte-for-byte with the ACTIVE release are
    // hard links instead of copies. /data is one filesystem, and the spotter
    // bundle's 24 MB of runtime + model rarely change while the 31 KB driver
    // does: without this, keeping active + previous would double 27 MB on a
    // device that has ~250 MB free.
    if let Some(active_id) = st.active.as_ref().map(|a| a.id.clone()) {
        let active_dir = paths.releases().join(&active_id);
        if active_dir.is_dir() {
            let (linked, saved) =
                dedup_shared_components(&staging, &verified.manifest, &active_dir);
            if linked > 0 {
                device::log_line(
                    &paths.log_path(),
                    &format!(
                        "install: {linked} file(s) shared with {active_id} by hard link ({saved} bytes not duplicated)"
                    ),
                );
            }
        }
    }

    // Fault-injection hook for the half-install drill (criterion ②): stall
    // after extraction, before the commit rename.
    if let Some(ms) = drill_stall_ms {
        device::log_line(
            &paths.log_path(),
            &format!("install: drill stall {ms} ms before commit"),
        );
        std::thread::sleep(Duration::from_millis(ms));
    }

    let manifest = verified.manifest.clone();
    let id = manifest.release_id();
    let target_dir = paths.releases().join(&id);
    if target_dir.exists() {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&incoming);
        return Err(format!(
            "release {id} is already installed; serials must be unique"
        ));
    }

    // 5. Commit: loose manifest+sig land with the components, then one rename.
    let write_loose = || -> Result<(), String> {
        state::write_atomic(&staging.join("manifest.json"), &verified.manifest_bytes)?;
        state::write_atomic(
            &staging.join("manifest.sig"),
            format!("{}\n", verified.signature_hex).as_bytes(),
        )
    };
    if let Err(e) = write_loose() {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&incoming);
        return Err(e);
    }
    package::sync_dir(&staging)?;
    std::fs::rename(&staging, &target_dir)
        .map_err(|e| format!("commit {}: {e}", target_dir.display()))?;
    package::sync_dir(&paths.releases())?;
    let _ = std::fs::remove_file(&incoming);
    device::log_line(
        &paths.log_path(),
        &format!(
            "install: {id} committed ({} bytes, serial {})",
            manifest.total_size, manifest.release.serial
        ),
    );

    // 6. Record the install (activation is a separate, explicit step).
    let now = state::now_ms();
    let mut run = LastRun {
        kind: "install".into(),
        status: "ok".into(),
        target: Some(state::release_ref(&manifest, now)),
        from: st.active.clone(),
        started_ms: now,
        ended_ms: now,
        ..Default::default()
    };
    run.criteria.push(state::Check {
        name: "signature".into(),
        ok: true,
        detail: format!(
            "Ed25519 verified, key fingerprint {}",
            crypto::fingerprint(&crypto::embedded_pubkey_hex())
        ),
    });
    run.criteria.push(state::Check {
        name: "hashes".into(),
        ok: true,
        detail: format!(
            "{} components verified (core, firmware, {}, console); total {} bytes",
            verified.manifest.top_level_files().len()
                + verified.manifest.components.console.files.len(),
            manifest.components.model.name,
            manifest.total_size
        ),
    });
    st.installer = Some(state::InstallerInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        binary: std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        updated_ms: now,
    });
    st.record(run);
    st.save(&paths.state_path())?;

    drop(_lock);
    if activate {
        crate::activate::run(paths, Some(id), false, "install --activate")
    } else {
        Ok(0)
    }
}

/// Device-dependent rejection: runs after the manifest parses, before any
/// component byte is extracted.
fn preflight(
    m: &Manifest,
    facts: &device::DeviceFacts,
    active_serial: Option<u64>,
) -> Result<(), String> {
    if m.target.model != facts.model {
        return Err(format!(
            "model mismatch: package targets {:?}, device reports {:?}",
            m.target.model, facts.model
        ));
    }
    if m.target.profile != facts.profile {
        return Err(format!(
            "profile mismatch: package targets {:?}, device is {:?}",
            m.target.profile, facts.profile
        ));
    }
    if facts.firmware_version.is_empty() {
        return Err(
            "device firmware version is unknown (no 'hm-firmware <ver>' marker in /mode)".into(),
        );
    }
    if !version_in_range(&facts.firmware_version, &m.firmware_range) {
        return Err(format!(
            "installed firmware {} is outside the package's supported range [{}, {}]",
            facts.firmware_version, m.firmware_range.min, m.firmware_range.max
        ));
    }
    if !version_in_range(&m.components.firmware.version, &m.firmware_range) {
        return Err(format!(
            "package firmware {} is outside its own declared range [{}, {}]",
            m.components.firmware.version, m.firmware_range.min, m.firmware_range.max
        ));
    }
    if let Some(active) = active_serial {
        if m.release.serial <= active {
            return Err(format!(
                "replay rejected: package serial {} is not newer than the active release's {}",
                m.release.serial, active
            ));
        }
    }
    Ok(())
}

/// The extracted payload must be what the manifest claims AND what this device
/// may run: ELF architecture (the tar hash proved the bytes; this proves they
/// can execute here), then the core's VAD admission.
///
/// The `hm-core-vad` marker is read here, from the extracted core, because it
/// is a fact about those bytes and no earlier gate has them — the manifest
/// parser never sees the core. This is still admission: nothing has been
/// renamed into `releases/`, so a rejection leaves the device untouched.
fn check_elf_inside(staging: &Path, m: &Manifest) -> Result<(), String> {
    let core = std::fs::read(staging.join(&m.components.core.file))
        .map_err(|e| format!("read core: {e}"))?;
    admit_core(&core)?;
    let fw = std::fs::read(staging.join(&m.components.firmware.file))
        .map_err(|e| format!("read firmware: {e}"))?;
    validate_elf(&fw, 40, 100 * 1024).map_err(|e| format!("firmware: {e}"))?;
    Ok(())
}

/// Install admission for the core payload: an aarch64 ELF that reports the fat
/// Silero backend. A slim or pre-marker core is refused exactly as `pack`
/// refuses it (same reader, `corebin`), so a package built with the emergency
/// `--allow-slim-core` drill cannot be installed by accident.
fn admit_core(bytes: &[u8]) -> Result<(), String> {
    validate_elf(bytes, 183, 1024 * 1024).map_err(|e| format!("core: {e}"))?;
    crate::corebin::require_fat_core(bytes).map_err(|e| format!("core: {e}"))
}

/// Make room before a package is fetched: staging/incoming leftovers from a
/// killed job, release directories other than the active/previous pair, and
/// DB snapshots beyond the two newest. Every removal is logged.
pub fn prune_before_install(paths: &Paths, st: &State) {
    let keep = st.keep_ids();
    let Ok(dir) = std::fs::read_dir(paths.releases()) else {
        return;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.starts_with(".staging-") || name.starts_with(".incoming-") {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            device::log_line(
                &paths.log_path(),
                &format!("prune: removed leftover {name}"),
            );
            continue;
        }
        // `current` is a symlink to the active release, not a release dir;
        // symlink_metadata keeps prune from following it.
        if name.starts_with('.') || name == "current" {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_dir() && !m.file_type().is_symlink() => {}
            _ => continue,
        }
        // With no active release (first install) every directory is a
        // potential rollback source; leave the layout alone.
        if st.active.is_none() {
            continue;
        }
        if keep.contains(&name) {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
        device::log_line(
            &paths.log_path(),
            &format!("prune: removed old release {name}"),
        );
    }
    prune_snapshots(&paths.snapshots(), 2);
}

fn prune_snapshots(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("db") {
                return None;
            }
            let ms = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Some((ms, p))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

/// Re-verify an on-disk release directory: signature over its loose manifest,
/// then every declared file's size and sha256. Used before activation so a
/// corrupted release directory can never be activated.
///
/// Division of labour, deliberately asymmetric: ADMISSION (`install`) decides
/// whether a package may enter `releases/` at all, and it reads the core's
/// `hm-core-vad` marker and the pinned model. ACTIVATION (this function) only
/// decides whether an already-installed directory is still intact, so it
/// checks bytes against the signed manifest and nothing else. Re-applying
/// admission here would make a device unable to roll back to, or even boot, a
/// release it once accepted — the manifest is already signed, so its claims
/// cannot change after the fact.
pub fn verify_release_dir(dir: &Path) -> Result<Manifest, String> {
    verify_release_dir_with_key(dir, &crypto::embedded_pubkey_hex())
}

fn verify_release_dir_with_key(dir: &Path, pubkey_hex: &str) -> Result<Manifest, String> {
    let manifest_bytes = std::fs::read(dir.join("manifest.json"))
        .map_err(|e| format!("{}: read manifest.json: {e}", dir.display()))?;
    let m = Manifest::from_bytes(&manifest_bytes)?;
    m.validate_structure()?;
    let sig = std::fs::read_to_string(dir.join("manifest.sig"))
        .map_err(|e| format!("{}: read manifest.sig: {e}", dir.display()))?;
    crypto::verify(&manifest_bytes, &sig, pubkey_hex)?;
    for (name, size, sha) in m.top_level_files() {
        check_file(&dir.join(name), size, sha)?;
    }
    let cdir = m.components.console.dir.trim_end_matches('/');
    for f in &m.components.console.files {
        let p = dir.join(cdir).join(&f.path);
        check_file(&p, f.size, &f.sha256)?;
    }
    if let Some(kws) = &m.components.kws {
        let kdir = kws.dir.trim_end_matches('/');
        for f in &kws.files {
            let p = dir.join(kdir).join(&f.path);
            check_file(&p, f.size, &f.sha256)?;
        }
    }
    Ok(m)
}

fn check_file(path: &Path, size: u64, sha: &str) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() != size {
        return Err(format!(
            "{}: size {} != declared {size}",
            path.display(),
            meta.len()
        ));
    }
    let got = crypto::sha256_file(path)?;
    if !got.eq_ignore_ascii_case(sha) {
        return Err(format!(
            "{}: sha256 mismatch (declared {sha}, got {got})",
            path.display()
        ));
    }
    Ok(())
}

/// Replace extracted files that match the active release with hard links.
/// Returns (files linked, bytes not duplicated). Failure to link is not fatal:
/// the extracted copy stays, which is exactly the pre-2026-11-15 behaviour.
fn dedup_shared_components(
    staging: &Path,
    m: &crate::manifest::Manifest,
    active_dir: &Path,
) -> (u64, u64) {
    let Some(kws) = &m.components.kws else {
        return (0, 0);
    };
    let root = kws.dir.trim_end_matches('/');
    let mut linked = 0u64;
    let mut saved = 0u64;
    for f in &kws.files {
        let rel = format!("{root}/{}", f.path);
        let staged = staging.join(&rel);
        let old = active_dir.join(&rel);
        let Ok(meta) = std::fs::metadata(&old) else {
            continue;
        };
        if meta.len() != f.size || !staged.is_file() {
            continue;
        }
        // The staged copy's hash is already proven by the package verification;
        // hash the active file once and compare. Worst case is 27 MB of hashing
        // for an install that just wrote the same 27 MB through sha256 anyway.
        let same = crypto::sha256_file(&old)
            .map(|h| h.eq_ignore_ascii_case(&f.sha256))
            .unwrap_or(false);
        if !same {
            continue;
        }
        let tmp = staged.with_extension("dedup");
        let _ = std::fs::remove_file(&tmp);
        if std::fs::hard_link(&old, &tmp).is_ok() {
            if std::fs::rename(&tmp, &staged).is_ok() {
                linked += 1;
                saved += f.size;
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    (linked, saved)
}

/// Pack a release directory into a package (used by the `pack` subcommand).
pub fn entries_for(dir: &Path, m: &Manifest) -> Result<Vec<PackEntry>, String> {
    let mut entries = Vec::new();
    let mode = |name: &str| {
        if name.ends_with("core-aarch64") || name.ends_with("firmware-armv7") {
            0o755
        } else {
            0o644
        }
    };
    for (name, _, _) in m.top_level_files() {
        entries.push(PackEntry {
            name: name.to_string(),
            source: dir.join(name),
            mode: mode(name),
        });
    }
    let cdir = m.components.console.dir.trim_end_matches('/');
    let mut files: Vec<&str> = m
        .components
        .console
        .files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    files.sort();
    for f in files {
        let name = format!("{cdir}/{f}");
        entries.push(PackEntry {
            name: name.clone(),
            source: dir.join(&name),
            mode: 0o644,
        });
    }
    // The spotter bundle (2026-11-15): every byte is executable under kws/ so
    // the loader, the driver, run.sh and the .so files all keep their bits.
    if let Some(kws) = &m.components.kws {
        let kdir = kws.dir.trim_end_matches('/');
        let mut kfiles: Vec<&str> = kws.files.iter().map(|f| f.path.as_str()).collect();
        kfiles.sort();
        for f in kfiles {
            let name = format!("{kdir}/{f}");
            entries.push(PackEntry {
                name: name.clone(),
                source: dir.join(&name),
                mode: crate::manifest::kws_file_mode(f, &kws.entry),
            });
        }
    }
    Ok(entries)
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

    /// A package whose core reports the slim backend is refused by install
    /// admission. `pack` refuses to build one without the emergency flag, so
    /// this is the second lock on the same door: a drill package that already
    /// exists on disk still cannot land on a device.
    #[test]
    fn admission_refuses_a_package_carrying_a_slim_core() {
        // The shape the release core has: aarch64 ELF64, just over the 1 MiB
        // floor `admit_core` enforces.
        let mut core = vec![0u8; 1024 * 1024 + 64];
        core[0..4].copy_from_slice(b"\x7fELF");
        core[4] = 2; // ELFCLASS64
        core[18] = 183; // EM_AARCH64
        let err = admit_core(&core).unwrap_err();
        assert!(err.contains("no 'hm-core-vad' marker"), "{err}");

        let slim = b"hm-core-vad none\0";
        core[4096..4096 + slim.len()].copy_from_slice(slim);
        let err = admit_core(&core).unwrap_err();
        assert!(err.starts_with("core: "), "{err}");
        assert!(err.contains("vad backend 'none'"), "{err}");

        // The fat core is the one that passes.
        let fat = b"hm-core-vad silero\0";
        core[4096..4096 + fat.len()].copy_from_slice(fat);
        admit_core(&core).expect("a fat core is admitted");
    }

    #[test]
    fn prune_removes_leftovers_old_releases_and_extra_snapshots() {
        let root = std::env::temp_dir().join(format!("hm-rel-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::for_test(root.join("hm"), root.join("fw"));
        paths.ensure_dirs().unwrap();
        for name in [
            "1-a",
            "2-b",
            "3-c",
            ".bootstrap",
            ".staging-9",
            ".incoming-9",
        ] {
            std::fs::create_dir_all(paths.releases().join(name)).unwrap();
        }
        std::fs::create_dir_all(paths.releases().join(".bootstrap")).unwrap();
        std::fs::write(paths.releases().join(".incoming-9/file"), b"x").ok();
        let snaps = paths.snapshots();
        for (i, name) in ["s1.db", "s2.db", "s3.db"].iter().enumerate() {
            let p = snaps.join(name);
            std::fs::write(&p, b"db").unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(3600 - i as u64);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        let st = State {
            active: Some(rel("1-a")),
            previous: Some(rel("2-b")),
            ..Default::default()
        };
        prune_before_install(&paths, &st);

        assert!(paths.releases().join("1-a").is_dir());
        assert!(paths.releases().join("2-b").is_dir());
        assert!(
            !paths.releases().join("3-c").exists(),
            "old release must go"
        );
        assert!(!paths.releases().join(".staging-9").exists());
        assert!(!paths.releases().join(".incoming-9").exists());
        assert!(
            paths.releases().join(".bootstrap").is_dir(),
            "adoption backup stays"
        );
        let left: Vec<String> = std::fs::read_dir(&snaps)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2);
        assert!(left.contains(&"s3.db".to_string()) && left.contains(&"s2.db".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_keeps_every_release_before_the_first_activation() {
        let root = std::env::temp_dir().join(format!("hm-rel-prune-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::for_test(root.join("hm"), root.join("fw"));
        paths.ensure_dirs().unwrap();
        for name in ["1-a", "2-b"] {
            std::fs::create_dir_all(paths.releases().join(name)).unwrap();
        }
        prune_before_install(&paths, &State::default());
        assert!(paths.releases().join("1-a").is_dir());
        assert!(paths.releases().join("2-b").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_release_dir_checks_kws_files() {
        use crate::manifest::{
            Components, ConsoleComponent, ConsoleFile, CoreComponent, FirmwareComponent,
            FirmwareRange, KwsComponent, KwsFile, ModelComponent, ReleaseInfo, Target,
            MANIFEST_VERSION,
        };
        use ed25519_dalek::SigningKey;
        use std::convert::TryInto;

        let seed = "07".repeat(32);
        let seed_bytes: [u8; 32] = crate::hexutil::from_hex(&seed).unwrap().try_into().unwrap();
        let pubkey = crate::hexutil::to_hex(
            SigningKey::from_bytes(&seed_bytes)
                .verifying_key()
                .as_bytes(),
        );

        let dir = std::env::temp_dir().join(format!("hm-rel-verify-kws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("components/web")).unwrap();
        std::fs::create_dir_all(dir.join("components/model")).unwrap();
        std::fs::create_dir_all(dir.join("components/kws")).unwrap();
        std::fs::write(dir.join("components/core-aarch64"), b"corebin").unwrap();
        std::fs::write(dir.join("components/firmware-armv7"), b"fwbin").unwrap();
        std::fs::write(dir.join("components/web/index.html"), b"html").unwrap();
        std::fs::write(dir.join("components/model/silero_vad.onnx"), b"mdl").unwrap();
        std::fs::write(dir.join("components/kws/run.sh"), b"#!/sh").unwrap();

        let m = Manifest {
            manifest_version: MANIFEST_VERSION,
            release: ReleaseInfo {
                serial: 1,
                version: "t".into(),
                created_ms: 1,
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
                    size: 7,
                    sha256: crypto::sha256_bytes(b"corebin"),
                    version: "0.0.1".into(),
                    build_id: "abc".into(),
                },
                firmware: FirmwareComponent {
                    file: "components/firmware-armv7".into(),
                    size: 5,
                    sha256: crypto::sha256_bytes(b"fwbin"),
                    version: "0.0.44".into(),
                    gateway_version: "0.0.44".into(),
                    build_id: "fw".into(),
                    roles: vec!["firmware".into()],
                },
                console: ConsoleComponent {
                    dir: "components/web".into(),
                    index: "index.html".into(),
                    source_commit: "t".into(),
                    files: vec![ConsoleFile {
                        path: "index.html".into(),
                        size: 4,
                        sha256: crypto::sha256_bytes(b"html"),
                    }],
                },
                model: ModelComponent {
                    file: "components/model/silero_vad.onnx".into(),
                    size: 3,
                    sha256: crypto::sha256_bytes(b"mdl"),
                    name: "silero_vad".into(),
                },
                kws: Some(KwsComponent {
                    dir: "components/kws".into(),
                    entry: "run.sh".into(),
                    files: vec![KwsFile {
                        path: "run.sh".into(),
                        size: 5,
                        sha256: crypto::sha256_bytes(b"#!/sh"),
                    }],
                }),
            },
            total_size: 7 + 5 + 4 + 3 + 5,
        };
        let mb = m.to_bytes();
        let sig = crypto::sign(&mb, &seed).unwrap();
        std::fs::write(dir.join("manifest.json"), &mb).unwrap();
        std::fs::write(dir.join("manifest.sig"), format!("{sig}\n")).unwrap();

        verify_release_dir_with_key(&dir, &pubkey).expect("intact release dir must verify");

        // Tamper with KWS file.
        std::fs::write(dir.join("components/kws/run.sh"), b"#!/xx").unwrap();
        let err = verify_release_dir_with_key(&dir, &pubkey).unwrap_err();
        assert!(err.contains("sha256 mismatch"), "{err}");

        // Delete KWS file.
        std::fs::remove_file(dir.join("components/kws/run.sh")).unwrap();
        let err = verify_release_dir_with_key(&dir, &pubkey).unwrap_err();
        assert!(err.contains("run.sh"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
