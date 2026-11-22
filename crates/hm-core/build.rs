//! Build identity for the release manifest.
//!
//! `HM_CORE_BUILD` is the identity `hm-release` puts in a manifest and the
//! health gate compares against `/status.build.id` — a semantic version alone
//! cannot tell two builds apart. Precedence: explicit env (CI), then
//! `git rev-parse --short=12 HEAD` plus a `-dirty` suffix, then `unknown`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HM_CORE_BUILD");
    // A new commit changes HEAD; rebuild so /status reports the current one.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    // The dirty flag describes the SOURCE TREE: any source edit must re-run
    // this script, or a cached clean id survives the edit and a dirty build
    // ships as clean (same defect measured on the firmware 2026-11-14).
    println!("cargo:rerun-if-changed=src");
    // Dependencies are build inputs too: a lockfile bump can rebuild the
    // binary without touching src and keep a stale clean id (audit 2026-11-14).
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.lock");

    let build = std::env::var("HM_CORE_BUILD")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            let mut id =
                git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
            let dirty = Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .map(|o| o.status.success() && !o.stdout.is_empty())
                .unwrap_or(false);
            if dirty && id != "unknown" {
                id.push_str("-dirty");
            }
            id
        });
    println!("cargo:rustc-env=HM_CORE_BUILD={build}");

    // The VAD backend is a BUILD FACT, not a runtime setting: a slim binary
    // (`--no-default-features`) has no in-process Silero model at all, so it
    // does not implement the release contract "fat hm-core + silero_vad.onnx
    // ship together". `hm-release pack` reads this marker out of the binary
    // and refuses anything but `silero` (see pack.rs). Cargo sets
    // CARGO_FEATURE_<NAME> for the package's own features.
    let vad = if std::env::var_os("CARGO_FEATURE_SILERO_VAD").is_some() {
        "silero"
    } else {
        "none"
    };
    println!("cargo:rustc-env=HM_CORE_VAD={vad}");
}
