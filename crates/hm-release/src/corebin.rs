//! Build facts read out of a core binary.
//!
//! `hm-core/build.rs` writes one `hm-core-vad <backend>` marker into
//! `.rodata`: `hm-core-vad silero\0` for the default (fat) feature set,
//! `hm-core-vad none\0` for `--no-default-features` (slim). That marker is a
//! BUILD fact, not a runtime setting, so both the packer and the device's
//! install admission read it from the bytes instead of trusting a flag.
//!
//! The marker is matched as the exact NUL-terminated byte pattern, never with
//! [`crate::device::extract_marker`]: link-time merging of string constants
//! can place the marker flush against the next `.rodata` string
//! (`hm-core-vad silero0.0.1...`), and a "printable run" reader would then
//! read the neighbour as part of the backend name.

/// What a core binary claims about its in-process VAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VadBackend {
    /// Exactly one `hm-core-vad silero` marker and no `none` marker: the fat
    /// build that the release contract requires.
    Silero,
    /// Exactly one `hm-core-vad none` marker and no `silero` marker: the slim
    /// build, a compile-time feature-isolation check and never a release
    /// component.
    Slim,
    /// No marker at all: a core built before the marker existed.
    Unknown,
    /// Both markers, or either one repeated. `build.rs` emits exactly one of
    /// the two, so this is not an artifact this repository produced — a
    /// spliced or mixed binary — and it must never be read as either backend.
    Conflicted,
}

impl VadBackend {
    /// Only the fat build may ship.
    pub fn is_fat(self) -> bool {
        self == VadBackend::Silero
    }

    /// One clause for an error message ("core binary <describe>").
    pub fn describe(self) -> &'static str {
        match self {
            VadBackend::Silero => "reports vad backend 'silero'",
            VadBackend::Slim => "reports vad backend 'none' (slim build)",
            VadBackend::Unknown => {
                "carries no 'hm-core-vad' marker (built before the marker existed)"
            }
            VadBackend::Conflicted => {
                "carries conflicting 'hm-core-vad' markers (both backends, or one repeated)"
            }
        }
    }
}

/// Count occurrences of `hm-core-vad <backend>\0`.
///
/// Counting rather than "first match wins" is what makes a binary carrying
/// both markers visible: a first-match reader silently picks whichever
/// backend happens to sit earlier in `.rodata`.
fn count_backend(bytes: &[u8], backend: &str) -> usize {
    let mut marker = b"hm-core-vad ".to_vec();
    marker.extend_from_slice(backend.as_bytes());
    marker.push(0);
    bytes
        .windows(marker.len())
        .filter(|w| *w == marker.as_slice())
        .count()
}

/// The backend a core binary claims, requiring exactly one coherent claim.
pub fn vad_backend(bytes: &[u8]) -> VadBackend {
    match (count_backend(bytes, "silero"), count_backend(bytes, "none")) {
        (1, 0) => VadBackend::Silero,
        (0, 1) => VadBackend::Slim,
        (0, 0) => VadBackend::Unknown,
        _ => VadBackend::Conflicted,
    }
}

/// Admission for a core binary on its way into a release or a device: only
/// the fat build passes.
///
/// The messages are separated on purpose. A missing marker is a stale
/// artifact whose fix is a rebuild; a `none` marker is a deliberate slim
/// build, which exists to prove the feature split compiles and is never a
/// migration path.
pub fn require_fat_core(bytes: &[u8]) -> Result<(), String> {
    match vad_backend(bytes) {
        VadBackend::Silero => Ok(()),
        VadBackend::Unknown => Err(
            "core carries no 'hm-core-vad' marker: it predates the fat-core gate, so its in-process \
             VAD is unknown; rebuild it with ./tools/build_core.sh (no --slim)"
                .into(),
        ),
        VadBackend::Slim => Err(
            "core reports vad backend 'none': `tools/build_core.sh --slim` is a compile-time \
             feature-isolation check, not a migration path, and a slim core cannot run the \
             silero_vad.onnx the release ships"
                .into(),
        ),
        VadBackend::Conflicted => Err(
            "core carries conflicting 'hm-core-vad' markers (both 'silero' and 'none', or one \
             repeated); refusing a mixed artifact"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_backend_is_read_from_its_nul_terminated_marker() {
        assert_eq!(
            vad_backend(b"...hm-core-vad silero\0..."),
            VadBackend::Silero
        );
        assert_eq!(vad_backend(b"...hm-core-vad none\0..."), VadBackend::Slim);
        assert_eq!(vad_backend(b"hm-core 0.0.1 abc\0"), VadBackend::Unknown);
        // Merged string: the marker has no terminator, so it claims nothing.
        assert_eq!(
            vad_backend(b"hm-core-vad silero0.0.1abc"),
            VadBackend::Unknown
        );
    }

    /// A binary carrying both markers is neither backend. This is the case
    /// that a first-match reader mislabels as fat.
    #[test]
    fn a_core_claiming_both_backends_is_conflicted() {
        let both = b"hm-core-vad none\0 ... hm-core-vad silero\0";
        assert_eq!(vad_backend(both), VadBackend::Conflicted);
        assert!(require_fat_core(both).unwrap_err().contains("conflicting"));
        // A repeated marker is the same class of defect.
        assert_eq!(
            vad_backend(b"hm-core-vad silero\0xhm-core-vad silero\0"),
            VadBackend::Conflicted
        );
    }

    #[test]
    fn admission_names_the_two_failure_kinds_differently() {
        let slim = require_fat_core(b"hm-core-vad none\0").unwrap_err();
        assert!(slim.contains("compile-time"), "{slim}");
        assert!(slim.contains("--slim"), "{slim}");
        let unknown = require_fat_core(b"no marker here").unwrap_err();
        assert!(unknown.contains("predates the fat-core gate"), "{unknown}");
        assert!(unknown.contains("rebuild"), "{unknown}");
        assert!(require_fat_core(b"hm-core-vad silero\0").is_ok());
    }
}
