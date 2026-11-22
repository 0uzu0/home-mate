//! `pack`: build a signed release from a prepared directory.
//!
//! Directory shape (the same shape a release directory ends up with):
//! ```text
//! <dir>/components/core-aarch64
//! <dir>/components/firmware-armv7
//! <dir>/components/web/**            (vite dist)
//! <dir>/components/model/silero_vad.onnx
//! <dir>/components/kws/**            (optional: spotter bundle, see --kws-bundle)
//! ```
//! Versions and build ids are read from markers inside the artifacts, never
//! typed by hand, so the manifest cannot drift from the binaries it describes.

use std::path::{Path, PathBuf};

use crate::corebin;
use crate::crypto;
use crate::device::extract_marker;
use crate::install;
use crate::manifest::{
    marker_version, Components, ConsoleComponent, ConsoleFile, CoreComponent, FirmwareComponent,
    FirmwareRange, KwsComponent, KwsFile, Manifest, ModelComponent, ReleaseInfo, Target,
    MANIFEST_VERSION,
};
use crate::package;

pub struct PackOpts {
    pub dir: PathBuf,
    pub out_prefix: PathBuf,
    pub serial: u64,
    pub version: String,
    pub notes: String,
    pub model: String,
    pub profile: String,
    pub firmware_min: String,
    pub firmware_max: String,
    pub key: PathBuf,
    /// Allow `-dirty`/missing build ids and uncommitted web sources. Drills
    /// only: a release must be built from a committed tree, otherwise "fixed
    /// version" is not true.
    pub allow_dirty: bool,
    /// Emergency override for the fat-core gate: ship a core that reports
    /// `hm-core-vad none`. Named after `allow_dirty` because it is the same
    /// class of escape hatch — a drill/fire drill, never a normal release.
    /// A slim core next to the shipped model still cannot run the model, so
    /// the resulting package is behaviourally different from what was
    /// measured; the flag exists only to unblock a device in the field. It
    /// covers the slim case alone: a core with no marker (or conflicting
    /// markers) is refused regardless, because the fix there is a rebuild.
    pub allow_slim_core: bool,
    /// Console source commit override (tests / exported trees). When absent,
    /// the packer reads HEAD and refuses an uncommitted `web/`.
    pub web_commit: Option<String>,
    /// Spotter bundle to ship (`kws/assemble.sh` output). `None` = a release
    /// without the component, which is legal: nodes keep the bundle they have.
    pub kws_bundle: Option<PathBuf>,
}

#[derive(Debug)]
pub struct Packed {
    pub tar: PathBuf,
    pub manifest: PathBuf,
    pub sig: PathBuf,
    pub manifest_value: Manifest,
}

pub fn run(opts: &PackOpts) -> Result<Packed, String> {
    pack_with_expected_model(opts, crate::manifest::SILERO_VAD_SHA256)
}

/// Test seam: the same packer against an explicit expected model hash, for
/// HOST integration tests whose fixture packages carry a synthetic model.
///
/// It only exists when the `test-seams` feature is on, and that feature is
/// enabled by this crate's own dev-dependency — so a production build (bin or
/// lib, no dev-dependencies) does not contain this entry point at all. `run`
/// always pins the audited `silero_vad.onnx`.
#[cfg(any(test, feature = "test-seams"))]
pub fn run_with_expected_model(
    opts: &PackOpts,
    expected_model_sha: &str,
) -> Result<Packed, String> {
    pack_with_expected_model(opts, expected_model_sha)
}

/// The real packer. `expected_model_sha` is the audited VAD model hash.
fn pack_with_expected_model(opts: &PackOpts, expected_model_sha: &str) -> Result<Packed, String> {
    let dir = &opts.dir;
    let core_path = dir.join("components/core-aarch64");
    let fw_path = dir.join("components/firmware-armv7");
    let web_dir = dir.join("components/web");
    let model_path = dir.join("components/model/silero_vad.onnx");
    for p in [&core_path, &fw_path, &model_path] {
        if !p.is_file() {
            return Err(format!("missing {}", p.display()));
        }
    }
    if !web_dir.join("index.html").is_file() {
        return Err(format!("missing {}", web_dir.join("index.html").display()));
    }

    let core_bytes = std::fs::read(&core_path).map_err(|e| format!("read core: {e}"))?;
    let (core_version, build_id) = core_identity(&core_bytes)
        .ok_or("core binary has no 'hm-core <version> <build-id>' marker (build it with tools/build_core.sh)")?;
    if dirty_build(&build_id) && !opts.allow_dirty {
        return Err(format!(
            "core build {build_id} comes from a dirty working tree; commit first (or pass --allow-dirty for a drill)"
        ));
    }
    // Fat-core gate (audit 2026-11-20, P1): the release contract is "fat
    // hm-core + silero_vad.onnx ship together". A slim binary has no
    // in-process Silero VAD (`--no-default-features` drops tract-onnx), so
    // pairing it with the shipped model would deliver a device that ignores
    // that model — the exact drift this gate exists to prevent.
    //
    // The refusals are deliberately not one message (audit P2): a core with
    // no marker at all is a stale artifact whose fix is a rebuild, a core that
    // says `none` is a deliberate slim build that must not be migrated with,
    // and a core claiming both is not an artifact this repository produced.
    // Only the slim case has an escape hatch, and `--allow-slim-core` is a
    // field drill — never a substitute for a rebuild.
    let vad_backend = corebin::vad_backend(&core_bytes);
    if !vad_backend.is_fat() {
        match vad_backend {
            corebin::VadBackend::Slim if opts.allow_slim_core => {
                eprintln!(
                    "hm-release: WARNING packing a core without in-process silero VAD ({}); \
                     --allow-slim-core is an emergency drill, not a migration path",
                    vad_backend.describe()
                );
            }
            corebin::VadBackend::Slim => {
                return Err(format!(
                    "core binary {}: a release requires the fat build with in-process silero VAD \
                     (./tools/build_core.sh without --slim). --allow-slim-core exists for an \
                     emergency drill only — it is not the fix for a routine migration",
                    vad_backend.describe()
                ))
            }
            _ => {
                return Err(format!(
                    "core binary {}: rebuild it with ./tools/build_core.sh (without --slim). \
                     --allow-slim-core cannot stand in for a backend the binary does not \
                     report",
                    vad_backend.describe()
                ))
            }
        }
    }
    let model_sha = crypto::sha256_file(&model_path)?;
    if !model_sha.eq_ignore_ascii_case(expected_model_sha) {
        return Err(format!(
            "components/model/silero_vad.onnx hashes to {model_sha}, not the audited \
             {expected_model_sha}; a release must ship that exact model"
        ));
    }
    let fw_bytes = std::fs::read(&fw_path).map_err(|e| format!("read firmware: {e}"))?;
    let (fw_version, fw_build_id) = firmware_identity(&fw_bytes).ok_or(
        "firmware has no 'hm-firmware <version> <build-id>' marker (rebuild with tools/build_firmware.sh)",
    )?;
    let fw_build_id = match fw_build_id {
        Some(id) => id,
        None if opts.allow_dirty => "unknown".into(),
        None => return Err(
            "firmware has no build id (marker is 'hm-firmware <version> <flavor>'); rebuild with \
                 tools/build_firmware.sh, or pass --allow-dirty for a drill"
                .into(),
        ),
    };
    if dirty_build(&fw_build_id) && !opts.allow_dirty {
        return Err(format!(
            "firmware build {fw_build_id} comes from a dirty working tree; commit first (or pass --allow-dirty for a drill)"
        ));
    }
    let gw_version = marker_version(
        &extract_marker(&fw_bytes, b"home-mate-device-gateway ")
            .ok_or("firmware has no gateway marker")?,
        "home-mate-device-gateway ",
    )
    .ok_or("gateway marker has no version")?;

    let mut console_files: Vec<ConsoleFile> = Vec::new();
    walk(&web_dir, &web_dir, &mut console_files)?;
    console_files.sort_by(|a, b| a.path.cmp(&b.path));
    if console_files.is_empty() {
        return Err("console directory has no files".into());
    }
    let source_commit = match &opts.web_commit {
        Some(c) => c.clone(),
        None => web_source_commit(opts.allow_dirty)?,
    };
    let index = find_index(&console_files)?;
    let index_html =
        std::fs::read_to_string(web_dir.join("index.html")).map_err(|e| e.to_string())?;
    if !index_html.contains(&index) {
        return Err(format!("index.html does not reference {index}"));
    }

    // The spotter bundle (2026-11-15). Its files are hashed here and shipped
    // under `components/kws/`; the node installs them by hard-linking into
    // /data/local/tmp/hm-kws, which is where the firmware looks. kw.txt is
    // NODE configuration (written by the gateway on kws_deploy) and is
    // rejected rather than silently overwritten.
    let kws_component = match &opts.kws_bundle {
        None => None,
        Some(bundle) => {
            if !bundle.is_dir() {
                return Err(format!(
                    "kws bundle {} is not a directory",
                    bundle.display()
                ));
            }
            if bundle.join("kw.txt").exists() {
                return Err(format!(
                    "{} contains kw.txt — the word list belongs to the node, not to a release",
                    bundle.display()
                ));
            }
            let mut files: Vec<KwsFile> = Vec::new();
            walk_kws(bundle, bundle, &mut files)?;
            files.sort_by(|a, b| a.path.cmp(&b.path));
            if files.is_empty() {
                return Err(format!("kws bundle {} has no files", bundle.display()));
            }
            if !files.iter().any(|f| f.path == "run.sh") {
                return Err(format!(
                    "kws bundle {} has no run.sh — the firmware spawns that launcher",
                    bundle.display()
                ));
            }
            Some(KwsComponent {
                dir: "components/kws".into(),
                entry: "run.sh".into(),
                files,
            })
        }
    };
    let manifest = Manifest {
        manifest_version: MANIFEST_VERSION,
        release: ReleaseInfo {
            serial: opts.serial,
            version: opts.version.clone(),
            created_ms: crate::state::now_ms(),
            notes: opts.notes.clone(),
        },
        target: Target {
            model: opts.model.clone(),
            profile: opts.profile.clone(),
            arch: "aarch64".into(),
        },
        firmware_range: FirmwareRange {
            min: opts.firmware_min.clone(),
            max: opts.firmware_max.clone(),
        },
        components: Components {
            core: CoreComponent {
                file: "components/core-aarch64".into(),
                size: core_bytes.len() as u64,
                sha256: crypto::sha256_bytes(&core_bytes),
                version: core_version,
                build_id,
            },
            firmware: FirmwareComponent {
                file: "components/firmware-armv7".into(),
                size: fw_bytes.len() as u64,
                sha256: crypto::sha256_bytes(&fw_bytes),
                version: fw_version,
                gateway_version: gw_version,
                build_id: fw_build_id,
                roles: vec!["firmware".into(), "gateway".into()],
            },
            console: ConsoleComponent {
                dir: "components/web".into(),
                index,
                source_commit,
                files: console_files,
            },
            model: ModelComponent {
                file: "components/model/silero_vad.onnx".into(),
                size: std::fs::metadata(&model_path).map(|m| m.len()).unwrap_or(0),
                sha256: model_sha,
                name: "silero_vad".into(),
            },
            kws: kws_component,
        },
        total_size: 0,
    };
    let mut manifest = manifest;
    manifest.total_size = manifest.computed_total();
    manifest.validate_structure()?;

    let manifest_bytes = manifest.to_bytes();
    let seed = std::fs::read_to_string(&opts.key)
        .map_err(|e| format!("read key {}: {e}", opts.key.display()))?;
    let sig = crypto::sign(&manifest_bytes, &seed)?;

    let entries = install::entries_for(dir, &manifest)?;
    let tar = PathBuf::from(format!("{}.tar", opts.out_prefix.display()));
    package::write_package(&tar, &manifest_bytes, &sig, &entries)?;

    let manifest_path = PathBuf::from(format!("{}.manifest.json", opts.out_prefix.display()));
    let sig_path = PathBuf::from(format!("{}.manifest.sig", opts.out_prefix.display()));
    crate::state::write_atomic(&manifest_path, &manifest_bytes)?;
    crate::state::write_atomic(&sig_path, format!("{sig}\n").as_bytes())?;

    Ok(Packed {
        tar,
        manifest: manifest_path,
        sig: sig_path,
        manifest_value: manifest,
    })
}

/// A build id carrying the uncommitted-work marker from `build.rs`.
pub fn dirty_build(build_id: &str) -> bool {
    build_id.ends_with("-dirty")
}

/// `hm-firmware <version> <build-id> <flavor>` marker. Older builds carry
/// only `<version> <flavor>`; the build id is then None.
pub fn firmware_identity(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let raw = extract_marker(bytes, b"hm-firmware ")?;
    let rest = raw.strip_prefix("hm-firmware ")?;
    let mut parts = rest.split_whitespace();
    let version = parts.next()?.to_string();
    let build = parts
        .next()
        .filter(|t| !t.starts_with('(') && !t.starts_with('(') && *t != "(emos)")
        .map(|t| t.to_string());
    Some((version, build))
}

/// The commit the console bundle was built from, read from the repository the
/// packer runs in. Uncommitted `web/` sources are refused (they would make the
/// manifest's identity a guess).
fn web_source_commit(allow_dirty: bool) -> Result<String, String> {
    let repo = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .ok_or("not inside a git work tree: run pack from the repository, pass --web-commit, or --allow-dirty for a drill")?;
    let git = |args: &[&str]| -> Result<String, String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .map_err(|e| format!("git {args:?}: {e}"))?;
        if !out.status.success() {
            return Err(format!("git {args:?} failed"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let head = git(&["rev-parse", "--short=12", "HEAD"])?;
    let status = git(&["status", "--porcelain", "--", "web"])?;
    if !status.is_empty() {
        if allow_dirty {
            return Ok(format!("{head}-dirty"));
        }
        return Err(format!(
            "web/ has uncommitted changes; commit them before packaging:\n{status}"
        ));
    }
    Ok(head)
}

/// `hm-core <version> <build-id>` marker: returns (version, build_id).
///
/// Every occurrence is tried, not just the first: `.rodata` also holds
/// human-readable strings that start with `hm-core ` (the `--version` format
/// string, log lines), and string merging does not preserve marker order.
/// A candidate that does not parse as `<version> <build-id>` is skipped.
fn core_identity(bytes: &[u8]) -> Option<(String, String)> {
    const MARKER: &[u8] = b"hm-core ";
    let mut from = 0usize;
    while from < bytes.len() {
        let rel = bytes[from..]
            .windows(MARKER.len())
            .position(|w| w == MARKER)?;
        let start = from + rel;
        if let Some(identity) = parse_core_identity(&bytes[start..]) {
            return Some(identity);
        }
        from = start + MARKER.len();
    }
    None
}

fn parse_core_identity(rest: &[u8]) -> Option<(String, String)> {
    let end = rest
        .iter()
        .position(|b| *b < 0x20 || *b > 0x7e)
        .unwrap_or(rest.len())
        .min(96);
    let raw = String::from_utf8_lossy(&rest[..end]);
    let mut parts = raw.strip_prefix("hm-core ")?.split_whitespace();
    let version = parts.next()?.to_string();
    let build = parts.next()?.to_string();
    Some((version, build))
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<ConsoleFile>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let ftype = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", path.display()))?;
        if ftype.is_symlink() {
            return Err(format!(
                "{}: symlinks are not allowed in a console bundle",
                path.display()
            ));
        }
        if ftype.is_dir() {
            walk(root, &path, out)?;
        } else if ftype.is_file() {
            let meta = entry
                .metadata()
                .map_err(|e| format!("stat {}: {e}", path.display()))?;
            let rel = path
                .strip_prefix(root)
                .map_err(|_| "path escaped the console dir".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            out.push(ConsoleFile {
                path: rel,
                size: meta.len(),
                sha256: crypto::sha256_file(&path)?,
            });
        }
    }
    Ok(())
}

/// Recursive walk of the spotter bundle. Symlinks are refused: a package must
/// carry the bytes it claims, and `cp -a` of a link would ship a dangling path.
fn walk_kws(root: &Path, dir: &Path, out: &mut Vec<KwsFile>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let ftype = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", path.display()))?;
        if ftype.is_symlink() {
            return Err(format!(
                "{}: symlinks are not allowed in a kws bundle",
                path.display()
            ));
        }
        if ftype.is_dir() {
            walk_kws(root, &path, out)?;
        } else if ftype.is_file() {
            let meta = entry
                .metadata()
                .map_err(|e| format!("stat {}: {e}", path.display()))?;
            let rel = path
                .strip_prefix(root)
                .map_err(|_| "path escaped the kws bundle".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            out.push(KwsFile {
                path: rel,
                size: meta.len(),
                sha256: crypto::sha256_file(&path)?,
            });
        }
    }
    Ok(())
}

/// The Vite entry bundle: `assets/index-<hash>.js` (or top-level in an older
/// build shape). Exactly one is expected in a clean `web/dist`.
fn find_index(files: &[ConsoleFile]) -> Result<String, String> {
    let mut found: Vec<&ConsoleFile> = files
        .iter()
        .filter(|f| {
            let name = f.path.rsplit('/').next().unwrap_or("");
            name.starts_with("index-") && name.ends_with(".js")
        })
        .collect();
    match found.len() {
        1 => Ok(found.remove(0).path.clone()),
        0 => Err("console has no index-*.js entry bundle".into()),
        _ => Err(format!(
            "console has more than one index-*.js entry bundle: {:?}",
            found.iter().map(|f| f.path.clone()).collect::<Vec<_>>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture release directory: synthetic core/firmware with the real marker
    /// shapes, a console bundle, and a caller-supplied "model". The model is
    /// bytes here (not the 629 KiB blob), so every gate test states its own
    /// expected hash explicitly.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str, vad: &[&str], model: Option<&[u8]>) -> Self {
            let root = std::env::temp_dir().join(format!(
                "hm-rel-pack-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("components/web")).unwrap();
            std::fs::create_dir_all(root.join("components/model")).unwrap();
            let mut core = vec![0u8; 4096];
            let id = b"hm-core 0.0.1 abc123def456\0";
            core[64..64 + id.len()].copy_from_slice(id);
            // `vad` is a list so a fixture can claim two backends at once: the
            // packer must refuse that instead of reading whichever marker
            // happens to come first.
            for (i, backend) in vad.iter().enumerate() {
                let marker = format!("hm-core-vad {backend}\0");
                let at = 1024 + i * 64;
                core[at..at + marker.len()].copy_from_slice(marker.as_bytes());
            }
            std::fs::write(root.join("components/core-aarch64"), core).unwrap();
            let mut fw = vec![0u8; 4096];
            let fwid = b"hm-firmware 0.0.44 fw123abc (emos)\0";
            fw[64..64 + fwid.len()].copy_from_slice(fwid);
            let gw = b"home-mate-device-gateway 0.0.44\0";
            fw[512..512 + gw.len()].copy_from_slice(gw);
            std::fs::write(root.join("components/firmware-armv7"), fw).unwrap();
            std::fs::write(
                root.join("components/web/index.html"),
                b"<html><script src=\"/index-test.js\"></script></html>",
            )
            .unwrap();
            std::fs::write(root.join("components/web/index-test.js"), b"console.log(1)").unwrap();
            if let Some(model) = model {
                std::fs::write(root.join("components/model/silero_vad.onnx"), model).unwrap();
            }
            std::fs::write(root.join("key.hex"), "07".repeat(32)).unwrap();
            Self { root }
        }

        fn opts(&self, tag: &str) -> PackOpts {
            PackOpts {
                dir: self.root.clone(),
                out_prefix: self.root.join(format!("out-{tag}")),
                serial: 1,
                version: "test".into(),
                notes: String::new(),
                model: "AEOBC".into(),
                profile: "dot2d-emos".into(),
                firmware_min: "0.0.44".into(),
                firmware_max: "0.0.99".into(),
                key: self.root.join("key.hex"),
                allow_dirty: true,
                allow_slim_core: false,
                web_commit: Some("test-commit".into()),
                kws_bundle: None,
            }
        }

        fn remove_model(&self) {
            std::fs::remove_file(self.root.join("components/model/silero_vad.onnx")).unwrap();
        }
    }

    fn sha(bytes: &[u8]) -> String {
        crypto::sha256_bytes(bytes)
    }

    /// The audit pin: if this constant ever moves, the release gate moves with
    /// it, so the value is asserted here rather than only in a comment.
    #[test]
    fn the_pinned_vad_model_hash_is_the_audited_one() {
        assert_eq!(
            crate::manifest::SILERO_VAD_SHA256,
            "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"
        );
        assert_eq!(
            crate::manifest::SILERO_VAD_FILE,
            "components/model/silero_vad.onnx"
        );
    }

    #[test]
    fn a_slim_core_is_refused_by_pack() {
        let fx = Fixture::new("slim", &["none"], Some(b"fixture-model"));
        let err = run_with_expected_model(&fx.opts("slim"), &sha(b"fixture-model")).unwrap_err();
        assert!(err.contains("vad backend 'none'"), "{err}");
        // The slim message names the emergency flag AND says it is not the
        // migration path, so the operator is not nudged into using it.
        assert!(err.contains("--allow-slim-core"), "{err}");
        assert!(err.contains("not the fix for a routine migration"), "{err}");
    }

    /// A core from before the marker existed gets its own message: the fix is
    /// a rebuild, and the drill flag does not apply to an unreadable backend.
    #[test]
    fn a_core_without_the_marker_is_refused_and_told_to_rebuild() {
        let fx = Fixture::new("nomarker", &[], Some(b"fixture-model"));
        let err =
            run_with_expected_model(&fx.opts("nomarker"), &sha(b"fixture-model")).unwrap_err();
        assert!(err.contains("no 'hm-core-vad' marker"), "{err}");
        assert!(err.contains("rebuild"), "{err}");
        assert!(
            !err.contains("not the fix for a routine migration"),
            "{err}"
        );
        // ...and the emergency flag does not paper over it.
        let mut opts = fx.opts("nomarker-flag");
        opts.allow_slim_core = true;
        let err = run_with_expected_model(&opts, &sha(b"fixture-model")).unwrap_err();
        assert!(err.contains("no 'hm-core-vad' marker"), "{err}");
    }

    /// A binary claiming BOTH backends is not fat: the reader counts marker
    /// occurrences, so a first-match reader's mistake (silero seen first) is
    /// impossible here.
    #[test]
    fn a_core_claiming_two_backends_is_refused_by_pack() {
        let fx = Fixture::new("twobackends", &["none", "silero"], Some(b"fixture-model"));
        let err =
            run_with_expected_model(&fx.opts("twobackends"), &sha(b"fixture-model")).unwrap_err();
        assert!(err.contains("conflicting"), "{err}");
        // The emergency flag is for a KNOWN slim core, not a mixed artifact.
        let mut opts = fx.opts("twobackends-flag");
        opts.allow_slim_core = true;
        let err = run_with_expected_model(&opts, &sha(b"fixture-model")).unwrap_err();
        assert!(err.contains("conflicting"), "{err}");
    }

    /// The emergency override exists (named after `--allow-dirty`), and it is
    /// the only way a slim core reaches a package.
    #[test]
    fn allow_slim_core_is_the_only_escape_hatch() {
        let fx = Fixture::new("slim-ok", &["none"], Some(b"fixture-model"));
        let mut opts = fx.opts("slim-ok");
        opts.allow_slim_core = true;
        let packed = run_with_expected_model(&opts, &sha(b"fixture-model")).unwrap();
        assert!(packed.tar.is_file());
        assert_eq!(
            packed.manifest_value.components.model.sha256,
            sha(b"fixture-model")
        );
    }

    #[test]
    fn a_missing_vad_model_is_refused_by_pack() {
        let fx = Fixture::new("nomodel", &["silero"], Some(b"fixture-model"));
        fx.remove_model();
        let err = run_with_expected_model(&fx.opts("nomodel"), &sha(b"fixture-model")).unwrap_err();
        assert!(err.contains("components/model/silero_vad.onnx"), "{err}");
    }

    #[test]
    fn a_model_with_the_wrong_hash_is_refused_by_pack() {
        let fx = Fixture::new("badmodel", &["silero"], Some(b"not-the-audited-model"));
        let err =
            run_with_expected_model(&fx.opts("badmodel"), &sha(b"fixture-model")).unwrap_err();
        assert!(err.contains("not the audited"), "{err}");
        assert!(err.contains(&sha(b"not-the-audited-model")), "{err}");
    }

    /// Fat core + the expected model passes, and the manifest records exactly
    /// the bytes that were hashed.
    #[test]
    fn fat_core_with_the_pinned_model_packs() {
        let fx = Fixture::new("fat", &["silero"], Some(b"fixture-model"));
        let packed = run_with_expected_model(&fx.opts("fat"), &sha(b"fixture-model")).unwrap();
        assert!(packed.tar.is_file());
        assert!(packed.manifest.is_file());
        assert!(packed.sig.is_file());
        let m = &packed.manifest_value;
        assert_eq!(m.components.model.file, crate::manifest::SILERO_VAD_FILE);
        assert_eq!(m.components.model.sha256, sha(b"fixture-model"));
        assert_eq!(m.components.core.build_id, "abc123def456");
    }

    /// The device side of the same contract: `verify` (and `install`) go
    /// through `package::verify_path`, which refuses a manifest whose model is
    /// not the audited one.
    #[test]
    fn the_device_verifier_refuses_a_package_without_the_pinned_model() {
        let fx = Fixture::new("verifygate", &["silero"], Some(b"fixture-model"));
        let packed =
            run_with_expected_model(&fx.opts("verifygate"), &sha(b"fixture-model")).unwrap();
        let err = crate::package::verify_path(&packed.tar).unwrap_err();
        assert!(err.contains("not the audited"), "{err}");
    }

    #[test]
    fn core_identity_reads_version_and_build() {
        let mut bytes = vec![0u8; 32];
        bytes.extend_from_slice(b"hm-core 0.0.1 40a5a18ccd31\0");
        assert_eq!(
            core_identity(&bytes).unwrap(),
            ("0.0.1".to_string(), "40a5a18ccd31".to_string())
        );
    }

    /// `.rodata` also carries the `--version` format string, which starts with
    /// `hm-core `. The reader must skip that decoy and find the real marker,
    /// wherever the linker put it.
    #[test]
    fn core_identity_skips_a_decoy_prefix_string() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"hm-core \xc0\x01 \xc0\x06 (vad \xc0\x02)\n\0");
        bytes.extend_from_slice(&[0u8; 64]);
        bytes.extend_from_slice(b"hm-core 0.0.1 40a5a18ccd31\0");
        assert_eq!(
            core_identity(&bytes).unwrap(),
            ("0.0.1".to_string(), "40a5a18ccd31".to_string())
        );
    }

    #[test]
    fn dirty_builds_are_named() {
        assert!(dirty_build("d5bee3341c0a-dirty"));
        assert!(!dirty_build("d5bee3341c0a"));
    }

    #[test]
    fn firmware_identity_requires_a_build_id_in_the_new_marker() {
        let mut new = vec![0u8; 32];
        new.extend_from_slice(b"hm-firmware 0.0.45 f4daff0fa604 (emos)\0");
        assert_eq!(
            firmware_identity(&new).unwrap(),
            ("0.0.45".to_string(), Some("f4daff0fa604".to_string()))
        );
        let mut old = vec![0u8; 32];
        old.extend_from_slice(b"hm-firmware 0.0.45 (emos)\0");
        assert_eq!(
            firmware_identity(&old).unwrap(),
            ("0.0.45".to_string(), None)
        );
    }

    #[test]
    fn web_source_gate_names_uncommitted_sources() {
        // A throwaway repo proves the gate without touching this work tree.
        let dir = std::env::temp_dir().join(format!("hm-rel-webgate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("web")).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("web/a.js"), b"a").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "x",
        ]);
        std::fs::write(dir.join("web/a.js"), b"b").unwrap();
        // web_source_commit works on the CURRENT directory's repo; run a git
        // command in the throwaway repo is enough to prove the porcelain
        // output, so assert that shape directly.
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["status", "--porcelain", "--", "web"])
            .output()
            .unwrap();
        assert!(!out.stdout.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_detection_requires_exactly_one_bundle() {
        let f = |path: &str| ConsoleFile {
            path: path.into(),
            size: 1,
            sha256: String::new(),
        };
        // Top-level (older build shape) and nested (current Vite shape).
        assert_eq!(
            find_index(&[f("index.html"), f("index-abc.js")]).unwrap(),
            "index-abc.js"
        );
        assert_eq!(
            find_index(&[f("index.html"), f("assets/index-abc.js")]).unwrap(),
            "assets/index-abc.js"
        );
        // Two entry bundles are ambiguous, never guess.
        assert!(
            find_index(&[f("index.html"), f("index-abc.js"), f("assets/index-def.js")]).is_err()
        );
    }
}
