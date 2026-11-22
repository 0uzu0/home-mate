//! Package I/O: deterministic tar writer + streaming verifier/extractor.
//!
//! The verifier never trusts a path, a size or an order in the archive:
//! `manifest.json` must be the first entry, `manifest.sig` the second, every
//! remaining entry must be a regular file declared by the manifest, extracted
//! bytes are capped by the declared size, and the signature is checked over
//! the exact manifest bytes read from the archive. Extraction (when asked for)
//! goes to a staging directory that is committed by the caller only after both
//! the hashes and the signature pass.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::crypto;
use crate::hexutil::to_hex;
use crate::manifest::Manifest;

pub const MANIFEST_ENTRY: &str = "manifest.json";
pub const SIG_ENTRY: &str = "manifest.sig";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SIG_BYTES: u64 = 4096;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Verified {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub signature_hex: String,
}

/// One file to place in the archive: (tar path, source path, unix mode).
pub struct PackEntry {
    pub name: String,
    pub source: PathBuf,
    pub mode: u32,
}

/// Deterministic writer: fixed order supplied by the caller, mtime 0,
/// uid/gid 0, no symlinks. `manifest.json` and `manifest.sig` go first so the
/// reader can decide whether to extract before touching the payload.
pub fn write_package(
    out: &Path,
    manifest_bytes: &[u8],
    signature_hex: &str,
    entries: &[PackEntry],
) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let file = std::fs::File::create(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(file));

    append_bytes(&mut builder, MANIFEST_ENTRY, manifest_bytes, 0o644)?;
    append_bytes(
        &mut builder,
        SIG_ENTRY,
        format!("{signature_hex}\n").as_bytes(),
        0o644,
    )?;
    for entry in entries {
        let meta = std::fs::metadata(&entry.source)
            .map_err(|e| format!("stat {}: {e}", entry.source.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", entry.source.display()));
        }
        let mut f = std::fs::File::open(&entry.source)
            .map_err(|e| format!("open {}: {e}", entry.source.display()))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(meta.len());
        header.set_mode(entry.mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, &entry.name, &mut f)
            .map_err(|e| format!("append {}: {e}", entry.name))?;
    }
    let mut writer = builder
        .into_inner()
        .map_err(|e| format!("finish archive: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("flush {}: {e}", out.display()))?;
    let file = writer
        .into_inner()
        .map_err(|e| format!("finish {}: {e}", out.display()))?;
    file.sync_all()
        .map_err(|e| format!("sync {}: {e}", out.display()))?;
    Ok(())
}

fn append_bytes<W: Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), String> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .map_err(|e| format!("append {name}: {e}"))
}

/// Validate a path inside the archive: relative, no `.`/`..`, no empty
/// components, no absolute paths, UTF-8. Returns the normalized string.
pub fn validate_rel_path(path: &Path) -> Result<String, String> {
    let s = path
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 entry path {:?}", path))?;
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        return Err("empty entry path".into());
    }
    if s.starts_with('/') || s.contains('\\') {
        return Err(format!("absolute or backslash entry path {s:?}"));
    }
    for part in s.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("unsafe entry path {s:?}"));
        }
    }
    Ok(s.to_string())
}

fn expected_for(m: &Manifest, path: &str) -> Option<(u64, String)> {
    for (name, size, sha) in m.top_level_files() {
        if name == path {
            return Some((size, sha.to_string()));
        }
    }
    let dir = m.components.console.dir.trim_end_matches('/');
    let prefix = format!("{dir}/");
    if let Some(rest) = path.strip_prefix(&prefix) {
        if let Some(f) = m.components.console.files.iter().find(|f| f.path == rest) {
            return Some((f.size, f.sha256.clone()));
        }
    }
    if let Some(kws) = &m.components.kws {
        let prefix = format!("{}/", kws.dir.trim_end_matches('/'));
        if let Some(rest) = path.strip_prefix(&prefix) {
            if let Some(f) = kws.files.iter().find(|f| f.path == rest) {
                return Some((f.size, f.sha256.clone()));
            }
        }
    }
    None
}

/// Read + verify a package. When `staging` is Some, files are also written
/// there (directories created, permissions 0755 for `core`/`firmware`, 0644
/// otherwise). `check` runs after the manifest parses and before any
/// component byte is written — device-dependent rejection happens there.
pub fn verify_reader<R: Read>(
    reader: R,
    staging: Option<&Path>,
    check: impl FnMut(&Manifest) -> Result<(), String>,
) -> Result<Verified, String> {
    verify_reader_with_key(reader, staging, check, &crypto::embedded_pubkey_hex())
}

/// Same as `verify_reader` but against an explicit public key. Tests use a
/// throwaway key; production always goes through the embedded one above.
pub fn verify_reader_with_key<R: Read>(
    reader: R,
    staging: Option<&Path>,
    mut check: impl FnMut(&Manifest) -> Result<(), String>,
    pubkey_hex: &str,
) -> Result<Verified, String> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|e| format!("tar: {e}"))?;

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut signature_hex: Option<String> = None;
    let mut manifest: Option<Manifest> = None;
    let mut seen: HashSet<String> = HashSet::new();
    let mut total_written: u64 = 0;
    let mut index = 0usize;

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let raw_path = entry
            .path()
            .map_err(|e| format!("tar path: {e}"))?
            .into_owned();
        let path = validate_rel_path(&raw_path)?;
        let etype = entry.header().entry_type();
        if etype.is_dir() {
            // Our writer emits no directory entries; tolerate them only when
            // they are inside the declared console directory.
            if index < 2 {
                return Err("directory entry before the manifest".into());
            }
            continue;
        }
        if !etype.is_file() {
            return Err(format!(
                "{path}: only regular files are allowed, found {etype:?}"
            ));
        }
        let size = entry.header().size().map_err(|e| format!("{path}: {e}"))?;

        match index {
            0 => {
                if path != MANIFEST_ENTRY {
                    return Err(format!(
                        "first entry must be {MANIFEST_ENTRY}, found {path}"
                    ));
                }
                if size > MAX_MANIFEST_BYTES {
                    return Err("manifest.json is larger than 1 MiB".into());
                }
                let mut buf = Vec::with_capacity(size as usize);
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("{path}: {e}"))?;
                let parsed = Manifest::from_bytes(&buf)?;
                parsed.validate_structure()?;
                check(&parsed)?;
                manifest_bytes = Some(buf);
                manifest = Some(parsed);
            }
            1 => {
                if path != SIG_ENTRY {
                    return Err(format!("second entry must be {SIG_ENTRY}, found {path}"));
                }
                if size > MAX_SIG_BYTES {
                    return Err("manifest.sig is larger than 4 KiB".into());
                }
                let mut buf = String::new();
                entry
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("{path}: {e}"))?;
                let mb = manifest_bytes
                    .as_deref()
                    .ok_or("manifest.sig before manifest.json")?;
                // Verify Ed25519 signature before extracting any payload file
                // to staging so an untrusted archive writes zero bytes to disk.
                crypto::verify(mb, &buf, pubkey_hex)?;
                signature_hex = Some(buf);
            }
            _ => {
                let m = manifest.as_ref().ok_or("payload before the manifest")?;
                let (want_size, want_sha) = expected_for(m, &path)
                    .ok_or_else(|| format!("{path}: not declared by the manifest"))?;
                if size != want_size {
                    return Err(format!(
                        "{path}: archive size {size} does not match the declared {want_size}"
                    ));
                }
                total_written += size;
                if total_written > MAX_TOTAL_BYTES {
                    return Err("archive grows past the 64 MiB cap".into());
                }
                let mut hasher = Sha256::new();
                let mut out_file = match staging {
                    Some(dir) => {
                        let target = dir.join(&path);
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent)
                                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                        }
                        // Executable: the two ELF slots and every file of the
                        // spotter bundle (loader, driver, .so files, run.sh).
                        // The bundle's dir is `components/kws`, so the check
                        // must use the manifest's prefix, not a bare "kws/".
                        let kws_mode = m.components.kws.as_ref().and_then(|k| {
                            let prefix = format!("{}/", k.dir.trim_end_matches('/'));
                            path.strip_prefix(&prefix)
                                .map(|rel| crate::manifest::kws_file_mode(rel, &k.entry))
                        });
                        let mode =
                            if path.ends_with("core-aarch64") || path.ends_with("firmware-armv7") {
                                0o755
                            } else if let Some(km) = kws_mode {
                                km
                            } else {
                                0o644
                            };
                        let f = create_with_mode(&target, mode)?;
                        Some((target, f))
                    }
                    None => None,
                };
                let mut buf = [0u8; 64 * 1024];
                let mut written = 0u64;
                loop {
                    let n = entry.read(&mut buf).map_err(|e| format!("{path}: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    written += n as u64;
                    if written > want_size {
                        return Err(format!("{path}: archive entry exceeds the declared size"));
                    }
                    hasher.update(&buf[..n]);
                    if let Some((_, f)) = out_file.as_mut() {
                        f.write_all(&buf[..n])
                            .map_err(|e| format!("write {path}: {e}"))?;
                    }
                }
                if written != want_size {
                    return Err(format!(
                        "{path}: short entry ({written} of {want_size} bytes)"
                    ));
                }
                let got = to_hex(&hasher.finalize());
                if !got.eq_ignore_ascii_case(&want_sha) {
                    return Err(format!(
                        "{path}: sha256 mismatch (declared {want_sha}, got {got})"
                    ));
                }
                if let Some((target, f)) = out_file {
                    f.sync_all()
                        .map_err(|e| format!("sync {}: {e}", target.display()))?;
                }
                seen.insert(path);
            }
        }
        index += 1;
    }

    let manifest = manifest.ok_or("package has no manifest.json")?;
    let manifest_bytes = manifest_bytes.ok_or("package has no manifest.json")?;
    let signature_hex = signature_hex.ok_or("package has no manifest.sig")?;
    if index < 3 {
        return Err("package has no component files".into());
    }

    // Every declared file must be present exactly as declared.
    for (name, _, _) in manifest.top_level_files() {
        if !seen.contains(name) {
            return Err(format!("package is missing {name}"));
        }
    }
    let dir = manifest.components.console.dir.trim_end_matches('/');
    for f in &manifest.components.console.files {
        let full = format!("{dir}/{}", f.path);
        if !seen.contains(&full) {
            return Err(format!("package is missing {full}"));
        }
    }
    if let Some(kws) = &manifest.components.kws {
        let kdir = kws.dir.trim_end_matches('/');
        for f in &kws.files {
            let full = format!("{kdir}/{}", f.path);
            if !seen.contains(&full) {
                return Err(format!("package is missing {full}"));
            }
        }
    }

    if let Some(dir) = staging {
        sync_dir(dir)?;
    }
    Ok(Verified {
        manifest,
        manifest_bytes,
        signature_hex: signature_hex.trim().to_string(),
    })
}

fn create_with_mode(path: &Path, mode: u32) -> Result<std::fs::File, String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    opts.open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))
}

pub fn sync_dir(dir: &Path) -> Result<(), String> {
    let f = std::fs::File::open(dir).map_err(|e| format!("open dir {}: {e}", dir.display()))?;
    f.sync_all()
        .map_err(|e| format!("sync dir {}: {e}", dir.display()))
}

/// Verify without writing anything (used by `verify` and by pack self-checks).
pub fn verify_path(path: &Path) -> Result<Verified, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    verify_for_device(std::io::BufReader::new(f), None, |_| Ok(()))
}

/// Device-side ADMISSION verify: the embedded public key, the pinned VAD
/// model, then the caller's own device check.
///
/// `verify` and `install` both go through here, so no package can be accepted
/// on a device without the audited `silero_vad.onnx` (see
/// `manifest::require_pinned_vad_model` for the rollback compatibility rule).
/// The core's own `hm-core-vad` marker is not visible at this point: only
/// `install` sees the extracted core bytes, and it admits them in
/// `install::check_elf_inside`.
/// `verify_reader` stays the raw format/hash/signature reader — package tests
/// and non-admission callers keep working on synthetic manifests.
pub fn verify_for_device<R: Read>(
    reader: R,
    staging: Option<&Path>,
    check: impl FnMut(&Manifest) -> Result<(), String>,
) -> Result<Verified, String> {
    verify_admission(reader, staging, check, crate::manifest::SILERO_VAD_SHA256)
}

/// Test seam: the admission verify against an explicit model hash, for HOST
/// fixtures whose packages carry a synthetic model. It exists only with the
/// `test-seams` feature (enabled by this crate's own dev-dependency), so a
/// production build has no such entry point; `verify_for_device` always pins
/// the audited hash.
#[cfg(any(test, feature = "test-seams"))]
pub fn verify_for_device_with_model<R: Read>(
    reader: R,
    staging: Option<&Path>,
    check: impl FnMut(&Manifest) -> Result<(), String>,
    expected_model_sha: &str,
) -> Result<Verified, String> {
    verify_admission(reader, staging, check, expected_model_sha)
}

/// Shared admission implementation: pinned model hash first, then the
/// caller's device check. `pub(crate)` on purpose — `install` reaches it with
/// its own expected hash, and no build outside this crate has a name for it.
pub(crate) fn verify_admission<R: Read>(
    reader: R,
    staging: Option<&Path>,
    mut check: impl FnMut(&Manifest) -> Result<(), String>,
    expected_model_sha: &str,
) -> Result<Verified, String> {
    verify_reader(reader, staging, move |m| {
        crate::manifest::require_vad_model_sha(m, expected_model_sha)?;
        check(m)
    })
}

#[cfg(test)]
fn verify_path_with_key(path: &Path, pubkey_hex: &str) -> Result<Verified, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    verify_reader_with_key(std::io::BufReader::new(f), None, |_| Ok(()), pubkey_hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        Components, ConsoleComponent, ConsoleFile, CoreComponent, FirmwareComponent, FirmwareRange,
        ModelComponent, ReleaseInfo, Target, MANIFEST_VERSION,
    };

    fn manifest_bytes() -> Vec<u8> {
        Manifest {
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
                    build_id: "abc123".into(),
                },
                firmware: FirmwareComponent {
                    file: "components/firmware-armv7".into(),
                    size: 5,
                    sha256: crypto::sha256_bytes(b"fwbin"),
                    version: "0.0.44".into(),
                    gateway_version: "0.0.44".into(),
                    build_id: "f4daff0fa604".into(),
                    roles: vec!["firmware".into()],
                },
                console: ConsoleComponent {
                    dir: "components/web".into(),
                    index: "index-x.js".into(),
                    source_commit: "test".into(),
                    files: vec![
                        ConsoleFile {
                            path: "index.html".into(),
                            size: 4,
                            sha256: crypto::sha256_bytes(b"html"),
                        },
                        ConsoleFile {
                            path: "index-x.js".into(),
                            size: 2,
                            sha256: crypto::sha256_bytes(b"js"),
                        },
                    ],
                },
                model: ModelComponent {
                    file: "components/model/silero_vad.onnx".into(),
                    size: 3,
                    sha256: crypto::sha256_bytes(b"mdl"),
                    name: "silero_vad".into(),
                },
                kws: None,
            },
            total_size: 7 + 5 + 4 + 2 + 3,
        }
        .to_bytes()
    }

    fn test_seed_hex() -> String {
        "07".repeat(32)
    }

    fn test_pubkey_hex() -> String {
        use ed25519_dalek::SigningKey;
        use std::convert::TryInto;
        let bytes: [u8; 32] = crate::hexutil::from_hex(&test_seed_hex())
            .unwrap()
            .try_into()
            .unwrap();
        crate::hexutil::to_hex(SigningKey::from_bytes(&bytes).verifying_key().as_bytes())
    }

    fn write_fixture(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::write(dir.join("core"), b"corebin").unwrap();
        std::fs::write(dir.join("fw"), b"fwbin").unwrap();
        std::fs::write(dir.join("web/index.html"), b"html").unwrap();
        std::fs::write(dir.join("web/index-x.js"), b"js").unwrap();
        std::fs::write(dir.join("model.onnx"), b"mdl").unwrap();
        std::fs::write(dir.join("manifest.json"), manifest_bytes()).unwrap();
        let sig = crypto::sign(&manifest_bytes(), &test_seed_hex()).unwrap();
        let entries = vec![
            PackEntry {
                name: "components/core-aarch64".into(),
                source: dir.join("core"),
                mode: 0o755,
            },
            PackEntry {
                name: "components/firmware-armv7".into(),
                source: dir.join("fw"),
                mode: 0o755,
            },
            PackEntry {
                name: "components/web/index.html".into(),
                source: dir.join("web/index.html"),
                mode: 0o644,
            },
            PackEntry {
                name: "components/web/index-x.js".into(),
                source: dir.join("web/index-x.js"),
                mode: 0o644,
            },
            PackEntry {
                name: "components/model/silero_vad.onnx".into(),
                source: dir.join("model.onnx"),
                mode: 0o644,
            },
        ];
        let tar_path = dir.join("pkg.tar");
        write_package(&tar_path, &manifest_bytes(), &sig, &entries).unwrap();
        tar_path
    }

    #[test]
    fn tampered_component_is_rejected() {
        let dir = std::env::temp_dir().join(format!("hm-rel-tamper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tar_path = write_fixture(&dir);
        // Flip one byte inside the core payload, then re-read.
        let mut raw = std::fs::read(&tar_path).unwrap();
        let pos = raw.windows(7).position(|w| w == b"corebin").unwrap();
        raw[pos] ^= 0x01;
        std::fs::write(dir.join("bad.tar"), &raw).unwrap();
        let err = verify_path_with_key(&dir.join("bad.tar"), &test_pubkey_hex()).unwrap_err();
        assert!(err.contains("sha256 mismatch"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_signature_is_rejected_even_with_good_hashes() {
        let dir = std::env::temp_dir().join(format!("hm-rel-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tar_path = write_fixture(&dir);
        // Verify with a different (still valid) key: hashes pass, signature must not.
        let other_pub = {
            use ed25519_dalek::SigningKey;
            use std::convert::TryInto;
            let bytes: [u8; 32] = crate::hexutil::from_hex(&"09".repeat(32))
                .unwrap()
                .try_into()
                .unwrap();
            crate::hexutil::to_hex(SigningKey::from_bytes(&bytes).verifying_key().as_bytes())
        };
        let err = verify_path_with_key(&tar_path, &other_pub).unwrap_err();
        assert!(err.contains("signature does not verify"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn good_package_verifies_and_extracts() {
        let dir = std::env::temp_dir().join(format!("hm-rel-good-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tar_path = write_fixture(&dir);
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let f = std::fs::File::open(&tar_path).unwrap();
        let v = verify_reader_with_key(
            std::io::BufReader::new(f),
            Some(&staging),
            |_| Ok(()),
            &test_pubkey_hex(),
        )
        .unwrap();
        assert_eq!(v.manifest.release.serial, 1);
        assert_eq!(
            std::fs::read(staging.join("components/web/index.html")).unwrap(),
            b"html"
        );
        assert!(staging.join("components/core-aarch64").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_signature_writes_zero_payload_files_to_staging() {
        let dir = std::env::temp_dir().join(format!("hm-rel-sig-zero-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tar_path = write_fixture(&dir);
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let other_pub = {
            use ed25519_dalek::SigningKey;
            use std::convert::TryInto;
            let bytes: [u8; 32] = crate::hexutil::from_hex(&"09".repeat(32))
                .unwrap()
                .try_into()
                .unwrap();
            crate::hexutil::to_hex(SigningKey::from_bytes(&bytes).verifying_key().as_bytes())
        };
        let f = std::fs::File::open(&tar_path).unwrap();
        let err = verify_reader_with_key(
            std::io::BufReader::new(f),
            Some(&staging),
            |_| Ok(()),
            &other_pub,
        )
        .unwrap_err();
        assert!(err.contains("signature does not verify"), "{err}");
        assert_eq!(
            std::fs::read_dir(&staging).unwrap().count(),
            0,
            "no payload bytes should be extracted before signature verification"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_kws_file_in_archive_is_rejected() {
        use crate::manifest::{KwsComponent, KwsFile};
        let dir = std::env::temp_dir().join(format!("hm-rel-kws-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::write(dir.join("core"), b"corebin").unwrap();
        std::fs::write(dir.join("fw"), b"fwbin").unwrap();
        std::fs::write(dir.join("web/index.html"), b"html").unwrap();
        std::fs::write(dir.join("web/index-x.js"), b"js").unwrap();
        std::fs::write(dir.join("model.onnx"), b"mdl").unwrap();
        let mut m = Manifest::from_bytes(&manifest_bytes()).unwrap();
        m.components.kws = Some(KwsComponent {
            dir: "components/kws".into(),
            entry: "run.sh".into(),
            files: vec![KwsFile {
                path: "run.sh".into(),
                size: 5,
                sha256: crypto::sha256_bytes(b"#!/sh"),
            }],
        });
        m.total_size += 5;
        let mb = m.to_bytes();
        let sig = crypto::sign(&mb, &test_seed_hex()).unwrap();
        let entries = vec![
            PackEntry {
                name: "components/core-aarch64".into(),
                source: dir.join("core"),
                mode: 0o755,
            },
            PackEntry {
                name: "components/firmware-armv7".into(),
                source: dir.join("fw"),
                mode: 0o755,
            },
            PackEntry {
                name: "components/web/index.html".into(),
                source: dir.join("web/index.html"),
                mode: 0o644,
            },
            PackEntry {
                name: "components/web/index-x.js".into(),
                source: dir.join("web/index-x.js"),
                mode: 0o644,
            },
            PackEntry {
                name: "components/model/silero_vad.onnx".into(),
                source: dir.join("model.onnx"),
                mode: 0o644,
            },
        ];
        let tar_path = dir.join("pkg-missing-kws.tar");
        write_package(&tar_path, &mb, &sig, &entries).unwrap();
        let err = verify_path_with_key(&tar_path, &test_pubkey_hex()).unwrap_err();
        assert!(
            err.contains("package is missing components/kws/run.sh"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        assert!(validate_rel_path(Path::new("../etc/passwd")).is_err());
        assert!(validate_rel_path(Path::new("/etc/passwd")).is_err());
        assert!(validate_rel_path(Path::new("a//b")).is_err());
        assert!(validate_rel_path(Path::new("components/web/index.html")).is_ok());
    }
}
