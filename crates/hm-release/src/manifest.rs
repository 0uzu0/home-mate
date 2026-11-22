//! Release manifest: the signed description of one application release.
//!
//! Field order here is the serialization order, and the signature is over the
//! exact bytes written by `to_bytes()`. The device verifies the same bytes it
//! read from the package — it never re-serializes before verifying.

use serde::{Deserialize, Serialize};

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub manifest_version: u32,
    pub release: ReleaseInfo,
    pub target: Target,
    pub firmware_range: FirmwareRange,
    pub components: Components,
    pub total_size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Monotonic install serial. A package with `serial <= installed` is a
    /// replay and is rejected; rollback re-activates an on-disk release.
    pub serial: u64,
    pub version: String,
    pub created_ms: u64,
    #[serde(default)]
    pub notes: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The model the device reports (`/api/v0/mode` → devices[].model).
    pub model: String,
    /// Deployment profile: `dot2d-emos` for the direct-occupation speaker.
    pub profile: String,
    /// Core architecture this release is built for (`aarch64`).
    pub arch: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FirmwareRange {
    pub min: String,
    pub max: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Components {
    pub core: CoreComponent,
    pub firmware: FirmwareComponent,
    pub console: ConsoleComponent,
    pub model: ModelComponent,
    /// The keyword-spotter runtime (driver + Debian loader + C/C++ runtime +
    /// sherpa-onnx + model + `run.sh`), 27 MB. Optional on purpose:
    /// - releases packaged before 2026-11-15 have none, and an installer that
    ///   finds none must leave the node's existing bundle alone;
    /// - an older installer reading a newer manifest ignores the field
    ///   (serde skips unknown fields), so a release carrying it still installs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kws: Option<KwsComponent>,
}

/// The spotter bundle. `entry` is the launcher the firmware spawns; `files`
/// lists every byte so the signature covers the whole thing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KwsComponent {
    /// Directory inside the package and inside the release dir (`kws`).
    pub dir: String,
    /// Launcher, relative to `dir` (`run.sh`).
    pub entry: String,
    pub files: Vec<KwsFile>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KwsFile {
    /// Path relative to `dir`, slash separated (`model/tokens.txt`).
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CoreComponent {
    pub file: String,
    pub size: u64,
    /// Real SHA-256 (not `fwupdate::sha256_prefix()`, which is FNV-1a).
    pub sha256: String,
    pub version: String,
    /// Build identity embedded in the binary and reported by `/status.build.id`.
    pub build_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FirmwareComponent {
    pub file: String,
    pub size: u64,
    pub sha256: String,
    /// `hm-firmware <version>` marker in the ELF.
    pub version: String,
    /// `home-mate-device-gateway <version>` marker in the same ELF.
    pub gateway_version: String,
    /// Build id from the `hm-firmware <version> <build-id>` marker. Empty in
    /// releases packaged before the firmware carried one; the packer now
    /// always writes it and refuses a dirty one.
    #[serde(default)]
    pub build_id: String,
    pub roles: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConsoleComponent {
    pub dir: String,
    /// Entry module the served index.html must reference (`index-*.js`).
    pub index: String,
    /// Source commit the console was built from (`<sha>` or `<sha>-dirty`).
    #[serde(default)]
    pub source_commit: String,
    pub files: Vec<ConsoleFile>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConsoleFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ModelComponent {
    pub file: String,
    pub size: u64,
    pub sha256: String,
    pub name: String,
}

/// The in-process VAD model every release must ship, and its audited hash.
///
/// The core no longer "degrades to the energy VAD when the model is missing":
/// the device fleet runs fat builds, so a package without THIS model would
/// silently install and then behave differently from the one that was
/// measured. Both `hm-release pack` and the device-side `verify` therefore
/// demand the model component and this exact sha256.
pub const SILERO_VAD_FILE: &str = "components/model/silero_vad.onnx";
pub const SILERO_VAD_SHA256: &str =
    "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6";

/// Reject a manifest whose model component is not the pinned Silero VAD file.
///
/// Compatibility note (verify is a DEVICE command): this is an ADMISSION
/// gate. It runs on `pack`, on `verify`, and on `install`, so a package built
/// before the pin cannot be installed again. It deliberately does NOT run in
/// `install::verify_release_dir`, which re-verifies an already-installed
/// release before activation/rollback — a device must stay able to roll back
/// to the release it already accepted, even if that release shipped another
/// model.
pub fn require_pinned_vad_model(m: &Manifest) -> Result<(), String> {
    require_vad_model_sha(m, SILERO_VAD_SHA256)
}

/// `pub(crate)` form for the crate's own callers: same checks, explicit hash.
/// The packer and the device verifier always pass the pinned constant; the
/// release builds use one admission path, and nothing outside this crate can
/// name this function.
pub(crate) fn require_vad_model_sha(m: &Manifest, expected_sha: &str) -> Result<(), String> {
    let model = &m.components.model;
    if model.file != SILERO_VAD_FILE {
        return Err(format!(
            "model component is '{}', expected '{SILERO_VAD_FILE}'",
            model.file
        ));
    }
    if !model.name.eq_ignore_ascii_case("silero_vad") {
        return Err(format!(
            "model component names '{}', expected 'silero_vad'",
            model.name
        ));
    }
    if !model.sha256.eq_ignore_ascii_case(expected_sha) {
        return Err(format!(
            "model sha256 {} is not the audited silero_vad.onnx ({expected_sha}); \
             a release must ship that exact model",
            model.sha256
        ));
    }
    Ok(())
}

impl Manifest {
    /// Canonical bytes: fixed field order, pretty-printed for inspection.
    /// Everything that signs or verifies uses these exact bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self)
            .expect("manifest serialization cannot fail")
            .into_iter()
            .chain(std::iter::once(b'\n'))
            .collect()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let m: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| format!("manifest is not valid JSON: {e}"))?;
        if m.manifest_version != MANIFEST_VERSION {
            return Err(format!(
                "manifest_version {} is not supported (device speaks {MANIFEST_VERSION})",
                m.manifest_version
            ));
        }
        Ok(m)
    }

    /// Directory name and human id: `<serial>-<version>`.
    pub fn release_id(&self) -> String {
        format!("{}-{}", self.release.serial, self.release.version)
    }

    /// Every non-console file with its declared (size, sha256), in a stable
    /// order. Console files are listed separately (they live under `dir`).
    pub fn top_level_files(&self) -> Vec<(&str, u64, &str)> {
        vec![
            (
                &self.components.core.file,
                self.components.core.size,
                &self.components.core.sha256,
            ),
            (
                &self.components.firmware.file,
                self.components.firmware.size,
                &self.components.firmware.sha256,
            ),
            (
                &self.components.model.file,
                self.components.model.size,
                &self.components.model.sha256,
            ),
        ]
    }

    /// Sum of every declared byte, recomputed from the component list. Used to
    /// reject a manifest whose `total_size` does not describe it.
    pub fn computed_total(&self) -> u64 {
        let mut total =
            self.components.core.size + self.components.firmware.size + self.components.model.size;
        if let Some(kws) = &self.components.kws {
            total += kws.files.iter().map(|f| f.size).sum::<u64>();
        }
        for f in &self.components.console.files {
            total += f.size;
        }
        total
    }

    /// Structural checks that must pass before any byte is committed to disk.
    /// Device-dependent checks (model/profile/firmware range) are separate.
    pub fn validate_structure(&self) -> Result<(), String> {
        if self.release.version.trim().is_empty() {
            return Err("release.version is empty".into());
        }
        if self.target.arch != "aarch64" {
            return Err(format!("target.arch {} is not aarch64", self.target.arch));
        }
        if self.computed_total() != self.total_size {
            return Err(format!(
                "total_size {} does not match the component list ({})",
                self.total_size,
                self.computed_total()
            ));
        }
        if self.total_size > 64 * 1024 * 1024 {
            return Err(format!(
                "package declares {} bytes (cap is 64 MiB)",
                self.total_size
            ));
        }
        for (name, size, sha) in self.top_level_files() {
            if !is_safe_rel_path(name) {
                return Err(format!("{name}: path must be a safe relative path"));
            }
            if size == 0 {
                return Err(format!("{name}: declared size is 0"));
            }
            if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("{name}: sha256 is not 64 hex chars"));
            }
        }
        if self.components.core.build_id.trim().is_empty() {
            return Err("core.build_id is empty".into());
        }
        let cdir = self.components.console.dir.trim_end_matches('/');
        if cdir.is_empty() || !is_safe_rel_path(cdir) {
            return Err("console.dir must be a non-empty relative path".into());
        }
        for f in &self.components.console.files {
            if !is_safe_rel_path(&f.path) {
                return Err(format!("console file '{}': path must be relative", f.path));
            }
        }
        if let Some(kws) = &self.components.kws {
            let kdir = kws.dir.trim_end_matches('/');
            if kdir.is_empty() || !is_safe_rel_path(kdir) {
                return Err("kws.dir must be a non-empty relative path".into());
            }
            if kws.files.is_empty() {
                return Err("kws.files is empty".into());
            }
            if !kws.files.iter().any(|f| f.path == kws.entry) {
                return Err(format!(
                    "kws.entry '{}' is not one of the declared files",
                    kws.entry
                ));
            }
            let mut seen: Vec<&str> = Vec::new();
            for f in &kws.files {
                if !is_safe_rel_path(&f.path) {
                    return Err(format!("kws file '{}': path must be relative", f.path));
                }
                // The word list is NODE configuration written by the gateway;
                // shipping one would overwrite a household's vocabulary on the
                // next release.
                if f.path == "kw.txt" || f.path.ends_with("/kw.txt") {
                    return Err("kws bundle must not contain kw.txt (node configuration)".into());
                }
                if f.size == 0 {
                    return Err(format!("kws file '{}': declared size is 0", f.path));
                }
                if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("kws file '{}': sha256 is not 64 hex chars", f.path));
                }
                if seen.contains(&f.path.as_str()) {
                    return Err(format!("kws file '{}' is listed twice", f.path));
                }
                seen.push(&f.path);
            }
        }
        if !self
            .components
            .console
            .files
            .iter()
            .any(|f| f.path == "index.html")
        {
            return Err("console.files does not contain index.html".into());
        }
        if self.components.console.index.trim().is_empty() {
            return Err("console.index is empty".into());
        }
        if !self
            .components
            .console
            .files
            .iter()
            .any(|f| f.path == self.components.console.index)
        {
            return Err(format!(
                "console.index {} is not one of console.files",
                self.components.console.index
            ));
        }
        Ok(())
    }
}

/// Safe relative path check (no leading slash, backslashes, empty segments, `.` or `..`).
pub fn is_safe_rel_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
}

/// Permissions mode for a file inside `components/kws/` (#238 P2-4):
/// executables, launcher scripts, and shared libraries get `0o755`; model
/// weights and text/JSON assets (`model/*.onnx`, `model/tokens.txt`) keep
/// `0o644` even when hard-linked into `/data/local/tmp/hm-kws/`.
pub fn kws_file_mode(rel_path: &str, entry: &str) -> u32 {
    if rel_path == entry {
        return 0o755;
    }
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    if name == "hm-kws"
        || name.ends_with(".sh")
        || name.ends_with(".so")
        || name.contains(".so.")
        || name.starts_with("ld-linux")
    {
        0o755
    } else {
        0o644
    }
}

/// Parse a dotted numeric version (first three fields; extra fields ignored).
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let token = s.trim().split_whitespace().next()?.trim_start_matches('v');
    let mut out = Vec::new();
    for part in token.split('.') {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        out.push(digits.parse().ok()?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Version inside a marker string, e.g. `hm-firmware 0.0.44 (emos)` → `0.0.44`.
pub fn marker_version(text: &str, marker: &str) -> Option<String> {
    let rest = text.split(marker).nth(1)?;
    Some(rest.split_whitespace().next()?.to_string())
}

pub fn cmp_version(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    Some(parse_version(a)?.cmp(&parse_version(b)?))
}

pub fn version_in_range(version: &str, range: &FirmwareRange) -> bool {
    match (
        cmp_version(version, &range.min),
        cmp_version(version, &range.max),
    ) {
        (Some(lo), Some(hi)) => {
            use std::cmp::Ordering::*;
            matches!(lo, Greater | Equal) && matches!(hi, Less | Equal)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            manifest_version: MANIFEST_VERSION,
            release: ReleaseInfo {
                serial: 1,
                version: "2026.11.13-1".into(),
                created_ms: 1794600000000,
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
                    size: 15815688,
                    sha256: "a".repeat(64),
                    version: "0.0.1".into(),
                    build_id: "40a5a18ccd31".into(),
                },
                firmware: FirmwareComponent {
                    file: "components/firmware-armv7".into(),
                    size: 6430680,
                    sha256: "b".repeat(64),
                    version: "0.0.44".into(),
                    gateway_version: "0.0.44".into(),
                    build_id: "f4daff0fa604".into(),
                    roles: vec!["firmware".into(), "gateway".into()],
                },
                console: ConsoleComponent {
                    dir: "components/web".into(),
                    index: "index-Hd32sjLs.js".into(),
                    source_commit: "213f0662d7a7".into(),
                    files: vec![
                        ConsoleFile {
                            path: "index.html".into(),
                            size: 1200,
                            sha256: "c".repeat(64),
                        },
                        ConsoleFile {
                            path: "index-Hd32sjLs.js".into(),
                            size: 4096,
                            sha256: "e".repeat(64),
                        },
                    ],
                },
                model: ModelComponent {
                    file: "components/model/silero_vad.onnx".into(),
                    size: 643854,
                    sha256: "d".repeat(64),
                    name: "silero_vad".into(),
                },
                kws: None,
            },
            total_size: 15815688 + 6430680 + 1200 + 4096 + 643854,
        }
    }

    #[test]
    fn canonical_bytes_round_trip() {
        let m = sample();
        let bytes = m.to_bytes();
        let back = Manifest::from_bytes(&bytes).unwrap();
        assert_eq!(m, back);
        // Deterministic: serializing again yields identical bytes.
        assert_eq!(bytes, back.to_bytes());
    }

    #[test]
    fn structure_checks_catch_mismatched_totals() {
        let mut m = sample();
        assert!(m.validate_structure().is_ok());
        m.total_size += 1;
        assert!(m.validate_structure().unwrap_err().contains("total_size"));
    }

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(version_in_range(
            "0.0.44",
            &FirmwareRange {
                min: "0.0.44".into(),
                max: "0.0.99".into()
            }
        ));
        assert!(
            version_in_range(
                "0.0.100",
                &FirmwareRange {
                    min: "0.0.44".into(),
                    max: "0.0.99".into()
                }
            ) == false
        );
        assert!(
            version_in_range(
                "0.0.9",
                &FirmwareRange {
                    min: "0.0.10".into(),
                    max: "0.0.99".into()
                }
            ) == false
        );
        assert_eq!(
            marker_version("hm-firmware 0.0.44 (emos)", "hm-firmware ").unwrap(),
            "0.0.44"
        );
    }
}

#[cfg(test)]
mod kws_manifest_tests {
    use super::*;

    fn base(kws: Option<KwsComponent>) -> Manifest {
        let mut m = Manifest {
            manifest_version: 1,
            release: ReleaseInfo {
                serial: 7,
                version: "v".into(),
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
                    size: 10,
                    sha256: "a".repeat(64),
                    version: "0.0.1".into(),
                    build_id: "abc".into(),
                },
                firmware: FirmwareComponent {
                    file: "components/firmware-armv7".into(),
                    size: 10,
                    sha256: "b".repeat(64),
                    version: "0.0.46".into(),
                    gateway_version: "0.0.46".into(),
                    build_id: "def".into(),
                    roles: vec!["firmware".into()],
                },
                console: ConsoleComponent {
                    dir: "components/web".into(),
                    index: "index-x.js".into(),
                    source_commit: "s".into(),
                    files: vec![
                        ConsoleFile {
                            path: "index.html".into(),
                            size: 1,
                            sha256: "c".repeat(64),
                        },
                        ConsoleFile {
                            path: "index-x.js".into(),
                            size: 1,
                            sha256: "c".repeat(64),
                        },
                    ],
                },
                model: ModelComponent {
                    file: "components/model/silero_vad.onnx".into(),
                    size: 10,
                    sha256: "d".repeat(64),
                    name: "silero_vad".into(),
                },
                kws,
            },
            total_size: 0,
        };
        m.total_size = m.computed_total();
        m
    }

    fn file(path: &str) -> KwsFile {
        KwsFile {
            path: path.into(),
            size: 5,
            sha256: "e".repeat(64),
        }
    }

    #[test]
    fn a_bundle_counts_toward_total_size_and_validates() {
        let m = base(Some(KwsComponent {
            dir: "components/kws".into(),
            entry: "run.sh".into(),
            files: vec![file("run.sh"), file("hm-kws")],
        }));
        // core + firmware + model + two console files + two bundle files
        assert_eq!(m.total_size, 10 + 10 + 10 + 1 + 1 + 5 + 5);
        m.validate_structure().expect("a complete bundle validates");
    }

    #[test]
    fn the_word_list_and_broken_paths_are_refused() {
        for (bad, needle) in [
            (file("kw.txt"), "kw.txt"),
            (file("model/kw.txt"), "kw.txt"),
            (file("../escape"), "relative"),
            (file("/abs"), "relative"),
        ] {
            let m = base(Some(KwsComponent {
                dir: "components/kws".into(),
                entry: "run.sh".into(),
                files: vec![file("run.sh"), bad.clone()],
            }));
            let err = m.validate_structure().unwrap_err();
            assert!(err.contains(needle), "{bad:?} -> {err}");
        }
    }

    #[test]
    fn the_entry_must_be_one_of_the_files() {
        let m = base(Some(KwsComponent {
            dir: "components/kws".into(),
            entry: "run.sh".into(),
            files: vec![file("hm-kws")],
        }));
        let err = m.validate_structure().unwrap_err();
        assert!(err.contains("kws.entry"), "{err}");
    }

    /// An OLD package (no kws) must still parse and validate: releases
    /// packaged before 2026-11-15 have to install on a new installer.
    #[test]
    fn a_manifest_without_the_bundle_still_validates() {
        let m = base(None);
        m.validate_structure()
            .expect("older manifests keep working");
        let bytes = m.to_bytes();
        let back = Manifest::from_bytes(&bytes).unwrap();
        assert_eq!(back.components.kws, None);
        // ...and it serializes without the field at all.
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("\"kws\""), "{text}");
    }

    #[test]
    fn kws_file_mode_distinguishes_executables_from_model_assets() {
        assert_eq!(kws_file_mode("run.sh", "run.sh"), 0o755);
        assert_eq!(kws_file_mode("hm-kws", "run.sh"), 0o755);
        assert_eq!(kws_file_mode("libsherpa-onnx-c-api.so", "run.sh"), 0o755);
        assert_eq!(kws_file_mode("libonnxruntime.so.1.17.1", "run.sh"), 0o755);
        assert_eq!(kws_file_mode("ld-linux-armhf.so.3", "run.sh"), 0o755);
        assert_eq!(kws_file_mode("model/tokens.txt", "run.sh"), 0o644);
        assert_eq!(kws_file_mode("model/encoder.int8.onnx", "run.sh"), 0o644);
        assert_eq!(kws_file_mode("config.json", "run.sh"), 0o644);
    }

    #[test]
    fn validate_structure_rejects_unsafe_component_paths() {
        let mut m1 = base(None);
        m1.components.core.file = "../core-aarch64".into();
        assert!(m1.validate_structure().is_err());

        let mut m2 = base(None);
        m2.components.console.dir = "/etc/web".into();
        assert!(m2.validate_structure().is_err());

        let mut m3 = base(None);
        m3.components.console.files[0].path = "../index.html".into();
        assert!(m3.validate_structure().is_err());
    }
}
