#!/usr/bin/env python3
"""Build a Home Mate bootstrap seed for one dot2 node (M9.5, 2026-11-14).

Design: docs/d/provision-design-20261114.md §2.1. The seed is the *cold start*
of the deployment mainline: `hm-release` cannot bootstrap a bare machine (it
asks the core for device facts before installing), so an empty node needs the
minimal runtime chain on disk first, and everything after that goes through
the signed release path.

What goes in (and why):

  hm/bin/core_a                 the core, from a SIGNED release (not built here)
  hm/hm-core-supervisor.sh      core respawn/slot logic (repo, proven on dot2-d)
  hm/web/                       the console, same release
  hm/data/models/silero_vad.onnx  the endpoint detector the core loads
  hm/bin/hm-release             the installer (release build only — it carries
                                the Ed25519 public key)
  hm-direct/audio_a             the firmware/gateway, same release
  hm-direct/device-id           THIS node's id; start_server.sh passes it as
                                --device-id so the gateway never comes up as
                                dot2-d
  hm-kws/                       the keyword sidecar bundle (sherpa runtime,
                                model, tokens, loader); kw.txt is EXCLUDED
  local/bin/start_server.sh     the supervisor emOS init execs

What never goes in: any database, state.json, led-map, acoustic calibration,
auth token, env.local or kw.txt. A seed that carries those clones an identity;
the node's own config and credentials are its own (M12a is the credential
store, not this).

Usage:
  tools/build_bootstrap.py --device-id dot2-f --release build/release/home-mate-release-2026.11.14-32.tar
  tools/build_bootstrap.py --device-id dot2-f --release ... --out /tmp/seed.tar.gz --json

Inputs that must exist (documented as prerequisites, not silently skipped):
  * a signed release package (verifies with hm-release before anything is read)
  * the KWS bundle (default ~/.home-mate/kws-build/bundle) — an artifact, not a
    git file: 27 MB of sherpa runtime + zipformer model. Its hashes are recorded
    in SEED.json so two seeds are comparable.
  * the aarch64 RELEASE build of hm-release (tools/build_release.sh).
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def resolve_gateway_dir(explicit: str | None = None) -> Path:
    """Resolve the companion Linux gateway repo (public first, then private dev fallback)."""
    if explicit:
        return Path(explicit)
    env_dir = os.environ.get("HM_GATEWAY_DIR", "").strip()
    if env_dir:
        return Path(env_dir)
    for name in ("home-mate-gateway-linux", "home-mate-device-gateway-b"):
        cand = REPO.parent / name
        if cand.is_dir():
            return cand
    return REPO.parent / "home-mate-gateway-linux"


DEFAULT_KWS = Path.home() / ".home-mate" / "kws-build" / "bundle"
DEFAULT_INSTALLER = REPO / "target/aarch64-unknown-linux-musl/release/hm-release"
REFERENCE_DEVICE = "dot2-d"  # the node this repo was developed on; ids must differ


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def die(msg: str) -> "NoReturn":
    print(f"build_bootstrap: {msg}", file=sys.stderr)
    sys.exit(1)


def verify_package(tool: Path, package: Path) -> dict:
    """Refuse to build a seed out of bytes nobody signed."""
    proc = subprocess.run(
        [str(tool), "verify", "--package", str(package)],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        die(f"release package failed verification:\n{proc.stdout}{proc.stderr}")
    print(proc.stdout.strip())
    return {}


def read_manifest(components_dir: Path) -> dict:
    manifest = components_dir / "manifest.json"
    if not manifest.is_file():
        die("extracted package has no manifest.json")
    return json.loads(manifest.read_text())


def stage_tree(seed: Path, args, release_components: Path, manifest: dict) -> dict:
    """Lay the seed out exactly as /data sees it after install.sh."""
    layout = {
        "hm/bin/core_a": release_components / "core-aarch64",
        "hm/hm-core-supervisor.sh": Path(args.core_supervisor),
        "hm/web": release_components / "web",
        "hm/data/models/silero_vad.onnx": release_components / "model" / "silero_vad.onnx",
        "hm-direct/audio_a": release_components / "firmware-armv7",
        "local/bin/start_server.sh": Path(args.supervisor),
    }
    if args.installer:
        layout["hm/bin/hm-release"] = Path(args.installer)

    for rel, src in layout.items():
        dst = seed / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        if not src.exists():
            die(f"missing input {src} (needed for {rel})")
        if src.is_dir():
            shutil.copytree(src, dst)
        else:
            shutil.copy2(src, dst)

    # Node identity: a file, not a patched supervisor (dot2-d and the repo stay
    # byte-identical).
    idfile = seed / "hm-direct/device-id"
    idfile.write_text(args.device_id + "\n")
    idfile.chmod(0o644)

    # The pre-synthesized wake acks (and the retreat ack) live in
    # `<data_dir>/assets/wake-acks/`. They are NOT in a release and NOT
    # regenerable without the household's TTS credentials, so a node that does
    # not get them here starts silent at the wake word (measured dot2-c
    # 2026-11-14: copied by hand from dot2-d). The tone file
    # `follow-window-end.wav` is deliberately NOT shipped: v0.24 replaced the
    # tone with the spoken cue, and v0.25 turned it off by default.
    assets = Path(args.assets_dir)
    ack_src = assets / "wake-acks"
    if (ack_src / "manifest.json").is_file():
        shutil.copytree(ack_src, seed / "hm/data/assets/wake-acks")
    elif getattr(args, "allow_empty_wake_acks", False):
        print(
            f"note: wake-ack assets absent at {ack_src}; continuing (--allow-empty-wake-acks). "
            "Configure TTS in http://<node-ip>:8930/services after first boot to synthesize wake acks."
        )
    else:
        die(
            f"wake-ack assets not found at {ack_src} (set --assets-dir, or pass --allow-empty-wake-acks "
            "for a brand-new household's first Dot 2 before TTS is configured).\n"
            "  They are pre-synthesized on a configured node and are not in git."
        )

    # KWS bundle minus kw.txt: the word list is this node's config, pushed by
    # the core. Also rewrite the sidecar's --name so logs name the right node.
    bundle = Path(args.kws_bundle)
    if not (bundle / "hm-kws").is_file():
        die(f"KWS bundle at {bundle} has no hm-kws driver (set --kws-bundle)")
    dst = seed / "hm-kws"
    dst.mkdir(parents=True, exist_ok=True)
    excluded = {"kw.txt"}
    for item in sorted(bundle.iterdir()):
        if item.name in excluded:
            continue
        target = dst / item.name
        if item.is_dir():
            shutil.copytree(item, target)
        else:
            shutil.copy2(item, target)
    run_sh = dst / "run.sh"
    if not run_sh.is_file():
        die("KWS bundle has no run.sh")
    # 2026-11-15: run.sh is node-independent — it reads the device id from
    # /data/local/tmp/hm-direct/device-id at start. Patching `--name` here is
    # gone on purpose: the same bundle now ships inside a signed release, and a
    # per-device file would make the two paths disagree.
    text = run_sh.read_text()
    if f"--name {args.device_id}" in text:
        die(
            "KWS bundle's run.sh still hardcodes --name; rebuild it with "
            "kws/assemble.sh (gateway repo) so the launcher derives the id"
        )
    run_sh.chmod(0o755)
    (dst / "hm-kws").chmod(0o755)

    # install.sh (tracked in the repo so it is reviewable on its own).
    installer = REPO / "deploy/bootstrap/install.sh"
    if not installer.is_file():
        die("deploy/bootstrap/install.sh missing")
    shutil.copy2(installer, seed / "install.sh")
    (seed / "install.sh").chmod(0o755)
    # The seed must be runnable as unpacked: init execs this path via
    # /system/bin/sh, but an operator may call it directly.
    (seed / "local/bin/start_server.sh").chmod(0o755)
    (seed / "hm/hm-core-supervisor.sh").chmod(0o755)

    return {
        "release": manifest.get("release", {}),
        "target": manifest.get("target", {}),
        "components": manifest.get("components", []),
    }


def file_records(seed: Path) -> list:
    out = []
    for path in sorted(seed.rglob("*")):
        if not path.is_file():
            continue
        rel = str(path.relative_to(seed))
        st = path.stat()
        out.append(
            {
                "path": rel,
                "size": st.st_size,
                "mode": oct(st.st_mode & 0o777),
                "sha256": sha256_file(path),
            }
        )
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description="Build a Home Mate bootstrap seed")
    ap.add_argument("--device-id", required=True, help="node id, e.g. dot2-f (must differ from dot2-d)")
    ap.add_argument("--release", required=True, help="signed release package (.tar)")
    ap.add_argument("--kws-bundle", default=str(DEFAULT_KWS), help=f"KWS bundle dir (default {DEFAULT_KWS})")
    ap.add_argument("--installer", default=str(DEFAULT_INSTALLER), help="aarch64 RELEASE hm-release")
    ap.add_argument(
        "--assets-dir",
        default=str(REPO / "crates/hm-core/data-test/assets"),
        help="dir holding wake-acks/ (pre-synthesized acks; not in git)",
    )
    ap.add_argument(
        "--allow-empty-wake-acks",
        action="store_true",
        help="allow building a first-node seed without pre-synthesized wake-acks/ (TTS will generate them after setup)",
    )
    ap.add_argument(
        "--gateway-dir",
        default=None,
        help="path to companion home-mate-gateway-linux checkout (overrides HM_GATEWAY_DIR / sibling auto-detect)",
    )
    ap.add_argument(
        "--supervisor",
        default=None,
        help="start_server.sh (defaults to <gateway-dir>/deploy/dot2d/emos/start_server.sh)",
    )
    ap.add_argument(
        "--core-supervisor",
        default=str(REPO / "deploy/speaker/hm-core-supervisor.sh"),
        help="hm-core-supervisor.sh",
    )
    ap.add_argument("--out", help="output tar.gz (default build/bootstrap/<id>-<release>.tar.gz)")
    ap.add_argument("--json", action="store_true", help="print SEED.json to stdout too")
    args = ap.parse_args()
    if not args.supervisor:
        args.supervisor = str(resolve_gateway_dir(args.gateway_dir) / "deploy/dot2d/emos/start_server.sh")

    if not args.device_id or not all(c.islower() or c.isdigit() or c == "-" for c in args.device_id):
        die("device id must be lowercase letters/digits/dash")
    if args.device_id == REFERENCE_DEVICE:
        die(
            f"--device-id {REFERENCE_DEVICE} is the reference node; two nodes with the same "
            "stable id are rejected by the hub (v0.19) — pick the new machine's id"
        )
    package = Path(args.release)
    if not package.is_file():
        die(f"release package not found: {package}")
    installer = Path(args.installer)
    if not installer.is_file():
        die(
            f"release installer not found: {installer}\n"
            "  build it with tools/build_release.sh (the debug build must never ship: it "
            "accepts HM_RELEASE_PUBKEY_OVERRIDE)"
        )

    host_tool = REPO / "target/debug/hm-release"
    if not host_tool.is_file():
        die("target/debug/hm-release missing — needed to verify the package on this host")

    print(f"package: {package} ({package.stat().st_size} bytes)")
    verify_package(host_tool, package)

    with tempfile.TemporaryDirectory(prefix="hm-seed-") as tmp:
        tmpdir = Path(tmp)
        with tarfile.open(package) as tf:
            tf.extractall(tmpdir, filter="data")
        components = tmpdir / "components"
        if not components.is_dir():
            die("package has no components/ directory")
        manifest = read_manifest(tmpdir)

        seed = tmpdir / f"home-mate-bootstrap-{args.device_id}"
        seed.mkdir()
        provenance = stage_tree(seed, args, components, manifest)

        files = file_records(seed)
        total = sum(f["size"] for f in files)
        seed_json = {
            "seed_version": 1,
            "created_ms": int(time.time() * 1000),
            "device_id": args.device_id,
            "source": {
                "release": provenance["release"],
                "target": provenance["target"],
                "package_sha256": sha256_file(package),
                "package_size": package.stat().st_size,
            },
            "kws_bundle": {
                "source_dir": str(Path(args.kws_bundle).resolve()),
                "excluded": ["kw.txt"],
                "note": "tokens/model/runtime only; the word list is pushed by the core",
            },
            "installer": {
                "path": "hm/bin/hm-release",
                "size": (seed / "hm/bin/hm-release").stat().st_size if (seed / "hm/bin/hm-release").is_file() else None,
                "sha256": sha256_file(seed / "hm/bin/hm-release") if (seed / "hm/bin/hm-release").is_file() else None,
            },
            "not_included": [
                "database (home-mate.db)",
                "state.json (mute/volume, slot pointers)",
                "led-map.json / acoustic calibration",
                "auth tokens",
                "env.local / api keys",
                "kw.txt (word list)",
            ],
            "total_size": total,
            "files": files,
        }
        (seed / "SEED.json").write_text(json.dumps(seed_json, ensure_ascii=False, indent=2) + "\n")

        out = Path(args.out) if args.out else (
            REPO / "build/bootstrap" / f"home-mate-bootstrap-{args.device_id}-{provenance['release'].get('version','unknown')}.tar.gz"
        )
        out.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(out, "w:gz") as tf:
            tf.add(seed, arcname=seed.name)
        (out.with_suffix(out.suffix + ".sha256")).write_text(
            f"{sha256_file(out)}  {out.name}\n"
        )

    rel = provenance["release"]
    comps = provenance["components"] or {}
    core = comps.get("core", {})
    fw = comps.get("firmware", {})
    print(f"seed:      {out} ({out.stat().st_size} bytes)")
    print(f"sha256:    {out.with_suffix(out.suffix + '.sha256').read_text().split()[0]}")
    print(f"device:    {args.device_id}")
    print(f"from:      release {rel.get('version')} serial {rel.get('serial')}")
    print(f"core:      {core.get('build_id')}   firmware: {fw.get('version')} {fw.get('build_id')}")
    print(f"contents:  {total} bytes across {len(files)} files")
    if args.json:
        print(json.dumps(seed_json, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
