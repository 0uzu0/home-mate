//! `hm-release` — Home Mate release verifier / installer / activator.
//!
//! This binary is the only component that verifies manifest signatures and the
//! only writer of the release pointers. The core spawns it (management API) and
//! the device supervisors are left untouched.

use hm_release::{activate, crypto, device, health, install, manifest, pack, package, state};

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
hm-release — Home Mate release tool (Ed25519 manifest, A/B activation)

Host (offline, dev Mac):
  keygen  --out <file>                         generate a signing key (seed, 0600)
  pubkey  [--key <file> | --embedded]          print a public key + fingerprint
  pack    --dir <dir> --out <prefix> --serial N --version V --key <file>
          [--notes TEXT] [--model AEOBC] [--profile dot2d-emos]
          [--firmware-min 0.0.44] [--firmware-max 0.0.99] [--allow-dirty]
          [--allow-slim-core]                    emergency drill: ship the SLIM core
                                                 (not a migration path; a core with
                                                 no marker is refused regardless)
          [--web-commit <sha>]                   override the console source commit
          [--kws-bundle <dir>]                   ship the keyword-spotter bundle
                                                 (kws/assemble.sh output)
  sign    --manifest <file> --key <file> --out <file>

Device:
  verify   --package <file>                    hashes + signature only, no writes
  install  --package <file|http-url> [--activate]
           [--drill-stall-ms N]                fault injection for the half-install drill
  activate [--id <release>] [--reason TEXT]    activate an installed release
  rollback [--reason TEXT]                     activate the recorded previous release
  status   [--json]                            active/previous/last from state.json
  health   [--json]                            evaluate health against the active release
  version

Environment: HM_RELEASE_ROOT (default /data/hm), HM_FW_DIR
(default /data/local/tmp/hm-direct), HM_RELEASE_CORE_URL (default
http://127.0.0.1:8930).
";

struct Args {
    cmd: String,
    flags: HashMap<String, String>,
    json: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        Self::parse_from(std::env::args().skip(1).collect())
    }

    /// Same parser over an explicit argv (the tests drive it directly; the
    /// old shape read `std::env::args()`, which is why the `--activate`
    /// breakage could only be found on the device).
    fn parse_from(argv: Vec<String>) -> Result<Self, String> {
        let mut it = argv.into_iter();
        let cmd = it.next().unwrap_or_else(|| "help".into());
        let mut flags = HashMap::new();
        let mut json = false;
        let mut args: Vec<String> = it.collect();
        let mut i = 0;
        while i < args.len() {
            let a = args[i].clone();
            if a == "--json" {
                json = true;
                i += 1;
                continue;
            }
            if let Some(rest) = a.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    flags.insert(k.to_string(), v.to_string());
                    i += 1;
                    continue;
                }
                // Boolean switches take no value. `--allow-dirty` used to be
                // read as "missing value"; `--activate` was still broken the
                // same way on 2026-11-14 (the core's `POST /release/install
                // {activate:true}` appended a bare `--activate` and got the
                // usage text), so both are listed explicitly.
                if matches!(
                    rest,
                    "allow-dirty" | "allow-slim-core" | "activate" | "embedded"
                ) {
                    flags.insert(rest.to_string(), "1".into());
                    i += 1;
                    continue;
                }
                let value = args.get(i + 1).cloned().unwrap_or_default();
                if value.is_empty() || value.starts_with("--") {
                    return Err(format!("--{rest} needs a value"));
                }
                flags.insert(rest.to_string(), value);
                i += 2;
                continue;
            }
            return Err(format!("unexpected argument {a:?}"));
        }
        args.clear();
        Ok(Self { cmd, flags, json })
    }

    fn get(&self, key: &str) -> Result<String, String> {
        self.flags
            .get(key)
            .cloned()
            .ok_or_else(|| format!("--{key} is required"))
    }

    fn opt(&self, key: &str) -> Option<String> {
        self.flags.get(key).cloned()
    }
}

fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hm-release: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let paths = device::Paths::from_env();
    match dispatch(&args, &paths) {
        Ok(code) => ExitCode::from(code.min(255) as u8),
        Err(e) => {
            eprintln!("hm-release: {e}");
            device::log_line(&paths.log_path(), &format!("error: {e}"));
            ExitCode::from(1)
        }
    }
}

fn dispatch(args: &Args, paths: &device::Paths) -> Result<i32, String> {
    match args.cmd.as_str() {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "version" | "--version" => {
            println!(
                "hm-release {} (manifest_version {})",
                env!("CARGO_PKG_VERSION"),
                manifest::MANIFEST_VERSION
            );
            println!(
                "public key fingerprint {}",
                crypto::fingerprint(&crypto::embedded_pubkey_hex())
            );
            Ok(0)
        }
        "keygen" => {
            let out = PathBuf::from(args.get("out")?);
            let (pub_hex, fp) = crypto::keygen(&out)?;
            println!(
                "private key: {} (mode 0600, keep it off the device)",
                out.display()
            );
            println!("public key:  {pub_hex}");
            println!("fingerprint: {fp}");
            println!(
                "put the public key in deploy/release/release-pubkey.hex and rebuild hm-release"
            );
            Ok(0)
        }
        "pubkey" => {
            if args.flags.contains_key("embedded") {
                let pub_hex = crypto::embedded_pubkey_hex();
                println!("{pub_hex}");
                println!("fingerprint: {}", crypto::fingerprint(&pub_hex));
                return Ok(0);
            }
            let key = PathBuf::from(args.get("key")?);
            // Derive the public key by signing nothing: the seed file is the
            // only input, and the signing key type is exposed through crypto.
            let pub_hex = crypto::pubkey_from_seed_file(&key)?;
            println!("{pub_hex}");
            println!("fingerprint: {}", crypto::fingerprint(&pub_hex));
            Ok(0)
        }
        "pack" => {
            let opts = pack::PackOpts {
                dir: PathBuf::from(args.get("dir")?),
                out_prefix: PathBuf::from(args.get("out")?),
                serial: args
                    .get("serial")?
                    .parse()
                    .map_err(|_| "serial must be a number".to_string())?,
                version: args.get("version")?,
                notes: args.opt("notes").unwrap_or_default(),
                model: args.opt("model").unwrap_or_else(|| "AEOBC".into()),
                profile: args.opt("profile").unwrap_or_else(|| "dot2d-emos".into()),
                firmware_min: args.opt("firmware-min").unwrap_or_else(|| "0.0.44".into()),
                firmware_max: args.opt("firmware-max").unwrap_or_else(|| "0.0.99".into()),
                key: PathBuf::from(args.get("key")?),
                allow_dirty: args.flags.contains_key("allow-dirty"),
                allow_slim_core: args.flags.contains_key("allow-slim-core"),
                web_commit: args.opt("web-commit"),
                kws_bundle: args.opt("kws-bundle").map(PathBuf::from),
            };
            let packed = pack::run(&opts)?;
            println!("package:  {}", packed.tar.display());
            println!("manifest: {}", packed.manifest.display());
            println!("sig:      {}", packed.sig.display());
            println!(
                "release {} serial {} — core {} build {}, firmware {} build {} / gateway {}, console {} @ {}, model {}",
                packed.manifest_value.release.version,
                packed.manifest_value.release.serial,
                packed.manifest_value.components.core.version,
                packed.manifest_value.components.core.build_id,
                packed.manifest_value.components.firmware.version,
                if packed.manifest_value.components.firmware.build_id.is_empty() {
                    "(none)"
                } else {
                    &packed.manifest_value.components.firmware.build_id
                },
                packed.manifest_value.components.firmware.gateway_version,
                packed.manifest_value.components.console.index,
                packed.manifest_value.components.console.source_commit,
                packed.manifest_value.components.model.sha256,
            );
            // Self-check with the device's own verifier before shipping.
            let v = package::verify_path(&packed.tar)?;
            println!(
                "self-check: signature + all hashes verified ({})",
                v.manifest.release_id()
            );
            Ok(0)
        }
        "sign" => {
            let manifest_path = PathBuf::from(args.get("manifest")?);
            let out = PathBuf::from(args.get("out")?);
            let key = PathBuf::from(args.get("key")?);
            let bytes = std::fs::read(&manifest_path)
                .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
            // Refuse to sign something that is not a well-formed manifest.
            let m = manifest::Manifest::from_bytes(&bytes)?;
            m.validate_structure()?;
            let seed = std::fs::read_to_string(&key)
                .map_err(|e| format!("read key {}: {e}", key.display()))?;
            let sig = crypto::sign(&bytes, &seed)?;
            state::write_atomic(&out, format!("{sig}\n").as_bytes())?;
            println!("signed {} → {}", manifest_path.display(), out.display());
            Ok(0)
        }
        "verify" => {
            let pkg = PathBuf::from(args.get("package")?);
            let v = package::verify_path(&pkg)?;
            let m = &v.manifest;
            println!(
                "OK  release {} serial {}",
                m.release.version, m.release.serial
            );
            println!(
                "    core {} build {} ({}), firmware {} / gateway {}, console {}, model {}",
                m.components.core.version,
                m.components.core.build_id,
                m.components.core.sha256,
                m.components.firmware.version,
                m.components.firmware.gateway_version,
                m.components.console.index,
                m.components.model.sha256
            );
            println!(
                "    target {} / {} / {}, firmware range [{}, {}], {} bytes",
                m.target.model,
                m.target.profile,
                m.target.arch,
                m.firmware_range.min,
                m.firmware_range.max,
                m.total_size
            );
            Ok(0)
        }
        "install" => {
            let drill = match args.opt("drill-stall-ms") {
                Some(v) => Some(
                    v.parse::<u64>()
                        .map_err(|_| "drill-stall-ms must be a number".to_string())?,
                ),
                None => None,
            };
            let activate = args.flags.contains_key("activate");
            install::run(paths, &args.get("package")?, activate, drill)
        }
        "activate" => activate::run(
            paths,
            args.opt("id"),
            false,
            &args.opt("reason").unwrap_or_else(|| "manual".into()),
        ),
        "rollback" => activate::run(
            paths,
            None,
            true,
            &args
                .opt("reason")
                .unwrap_or_else(|| "manual rollback".into()),
        ),
        "status" => {
            let st = state::State::load(&paths.state_path())?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?
                );
            } else {
                print_status(&st);
                if let Some(free) = device::free_bytes(&paths.releases()) {
                    println!(
                        "disk:     {} bytes free under {}",
                        free,
                        paths.releases().display()
                    );
                }
            }
            Ok(0)
        }
        "health" => {
            let st = state::State::load(&paths.state_path())?;
            let active = st.active.ok_or("no active release in state.json")?;
            let want = health::Expect::from_ref(&active);
            let report = health::evaluate(&paths, &want);
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report.checks).unwrap_or_default()
                );
            } else {
                for c in &report.checks {
                    println!(
                        "{} {:16} {}",
                        if c.ok { "PASS" } else { "FAIL" },
                        c.name,
                        c.detail
                    );
                }
                println!("{}", if report.ok { "healthy" } else { "UNHEALTHY" });
            }
            Ok(if report.ok { 0 } else { 1 })
        }
        other => {
            eprintln!("hm-release: unknown command {other:?}\n\n{USAGE}");
            Ok(2)
        }
    }
}

fn print_status(st: &state::State) {
    match &st.active {
        Some(a) => println!(
            "active:   {} (serial {}, core build {}, firmware {}/{}, console {})",
            a.id, a.serial, a.build_id, a.firmware_version, a.gateway_version, a.console_index
        ),
        None => println!("active:   none (no release has been activated)"),
    }
    match &st.previous {
        Some(p) => println!("previous: {} (serial {})", p.id, p.serial),
        None => println!("previous: none"),
    }
    if let Some(l) = &st.last {
        println!(
            "last:     {} {} target {} → {}",
            l.kind,
            l.status,
            l.target.as_ref().map(|t| t.id.as_str()).unwrap_or("-"),
            l.error.as_deref().unwrap_or("ok")
        );
        for c in &l.criteria {
            println!(
                "          {} {:16} {}",
                if c.ok { "PASS" } else { "FAIL" },
                c.name,
                c.detail
            );
        }
        if let Some(db) = &l.db {
            println!(
                "          db: snapshot={:?} schema {:?}→{:?} restored={} {}",
                db.snapshot, db.schema_before, db.schema_after, db.restored, db.note
            );
        }
    }
    if let Some(g) = st.installer.as_ref() {
        println!("installer: hm-release {} at {}", g.version, g.binary);
    }
}

#[cfg(test)]
mod tests {
    use super::Args;

    /// `unwrap()` on a Result needs the error type to be printable only;
    /// `Args` itself never has to be Debug except in the expect message.

    /// 2026-11-14: `POST /api/v0/release/install {"activate":true}` was
    /// broken because the parser read `--activate` as a value option and
    /// returned "--activate needs a value". The core appends a BARE flag.
    #[test]
    fn boolean_switches_take_no_value() {
        let a = Args::parse_from(vec![
            "install".into(),
            "--package".into(),
            "home-mate-release.tar".into(),
            "--activate".into(),
        ])
        .expect("a bare --activate must parse");
        assert_eq!(a.cmd, "install");
        assert_eq!(a.get("package").unwrap(), "home-mate-release.tar");
        assert!(a.flags.contains_key("activate"));

        let p = Args::parse_from(vec![
            "pack".into(),
            "--dir".into(),
            "d".into(),
            "--allow-dirty".into(),
        ])
        .unwrap();
        assert!(p.flags.contains_key("allow-dirty"));
    }

    /// The value options keep their guard: a flag at the end of the line is
    /// a missing value, not a value.
    #[test]
    fn value_options_still_require_a_value() {
        match Args::parse_from(vec!["pack".into(), "--dir".into()]) {
            Ok(_) => panic!("a flag with no value must not parse"),
            Err(e) => assert!(e.contains("--dir needs a value"), "{e}"),
        }
    }
}
