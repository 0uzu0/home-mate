#!/usr/bin/env python3
"""Unit tests for Dot 2 provisioning script public/private gateway resolution."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import types
import unittest

REPO = Path(__file__).resolve().parent.parent
HM_PROVISION = REPO / "tools/hm-provision.sh"
BUILD_BOOTSTRAP = REPO / "tools/build_bootstrap.py"


def load_build_bootstrap():
    spec = importlib.util.spec_from_file_location("build_bootstrap", BUILD_BOOTSTRAP)
    mod = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(mod)
    return mod


class TestDot2ProvisioningResolution(unittest.TestCase):
    def test_hm_provision_shell_syntax_and_help(self):
        syn = subprocess.run(["sh", "-n", str(HM_PROVISION)], text=True, capture_output=True, timeout=10)
        self.assertEqual(syn.returncode, 0, syn.stderr)
        hlp = subprocess.run(["sh", str(HM_PROVISION), "--help"], text=True, capture_output=True, timeout=10)
        self.assertEqual(hlp.returncode, 0, hlp.stderr)
        self.assertIn("hm-provision.sh", hlp.stdout)

    def test_hm_provision_resolve_serial_tool_precedence(self):
        # Extract the exact resolve_serial_tool function from hm-provision.sh and run it
        # in an isolated directory layout covering HM_GATEWAY_DIR, public, and private siblings.
        src = HM_PROVISION.read_text()
        start = src.index("resolve_serial_tool()")
        end = src.index("SERIAL_TOOL=$(resolve_serial_tool)", start)
        func_body = src[start:end]

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            core = root / "home-mate"
            (core / "tools").mkdir(parents=True)
            pub_gw = root / "home-mate-gateway-linux"
            priv_gw = root / "home-mate-device-gateway-b"
            custom_gw = root / "custom-gw"
            for d in (pub_gw, priv_gw, custom_gw):
                (d / "tools").mkdir(parents=True)

            def run_resolve(env_extra=None):
                env = {k: v for k, v in os.environ.items() if k != "HM_GATEWAY_DIR"}
                if env_extra:
                    env.update(env_extra)
                script = f'HERE="{core}"\n{func_body}\nresolve_serial_tool\n'
                res = subprocess.run(["sh", "-eu"], input=script, text=True, env=env, capture_output=True, timeout=10)
                self.assertEqual(res.returncode, 0, res.stderr)
                return res.stdout.strip()

            # 1. No candidate present -> falls back to public home-mate-gateway-linux path
            self.assertEqual(run_resolve(), str(core / "../home-mate-gateway-linux/tools/serial_cmd.py"))

            # 2. Private fallback works when only private repo exists
            (priv_gw / "tools/serial_cmd.py").write_text("#!/usr/bin/env python3\n")
            self.assertEqual(run_resolve(), str(core / "../home-mate-device-gateway-b/tools/serial_cmd.py"))

            # 3. Public sibling takes precedence over private sibling
            (pub_gw / "tools/serial_cmd.py").write_text("#!/usr/bin/env python3\n")
            self.assertEqual(run_resolve(), str(core / "../home-mate-gateway-linux/tools/serial_cmd.py"))

            # 4. Explicit HM_GATEWAY_DIR overrides siblings
            (custom_gw / "tools/serial_cmd.py").write_text("#!/usr/bin/env python3\n")
            self.assertEqual(
                run_resolve({"HM_GATEWAY_DIR": str(custom_gw)}),
                str(custom_gw / "tools/serial_cmd.py"),
            )

    def test_build_bootstrap_resolve_gateway_dir_and_cli_flags(self):
        mod = load_build_bootstrap()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_repo = root / "home-mate"
            fake_repo.mkdir()
            orig_repo = mod.REPO
            orig_env = os.environ.pop("HM_GATEWAY_DIR", None)
            try:
                mod.REPO = fake_repo
                # 1. Neither exists -> defaults to public repo name
                self.assertEqual(mod.resolve_gateway_dir(), root / "home-mate-gateway-linux")
                # 2. Private fallback works
                (root / "home-mate-device-gateway-b").mkdir()
                self.assertEqual(mod.resolve_gateway_dir(), root / "home-mate-device-gateway-b")
                # 3. Public sibling preferred over private
                (root / "home-mate-gateway-linux").mkdir()
                self.assertEqual(mod.resolve_gateway_dir(), root / "home-mate-gateway-linux")
                # 4. HM_GATEWAY_DIR overrides sibling
                os.environ["HM_GATEWAY_DIR"] = str(root / "env-gw")
                self.assertEqual(mod.resolve_gateway_dir(), root / "env-gw")
                # 5. Explicit --gateway-dir overrides HM_GATEWAY_DIR
                self.assertEqual(mod.resolve_gateway_dir(str(root / "cli-gw")), root / "cli-gw")
            finally:
                mod.REPO = orig_repo
                os.environ.pop("HM_GATEWAY_DIR", None)
                if orig_env is not None:
                    os.environ["HM_GATEWAY_DIR"] = orig_env

        hlp = subprocess.run(["python3", str(BUILD_BOOTSTRAP), "--help"], text=True, capture_output=True, timeout=10)
        self.assertEqual(hlp.returncode, 0, hlp.stderr)
        self.assertIn("--gateway-dir", hlp.stdout)
        self.assertIn("--allow-empty-wake-acks", hlp.stdout)

    def test_stage_tree_allow_empty_wake_acks_behavior(self):
        mod = load_build_bootstrap()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            rel_comp = root / "release-components"
            (rel_comp / "web").mkdir(parents=True)
            (rel_comp / "model").mkdir(parents=True)
            (rel_comp / "core-aarch64").write_bytes(b"core")
            (rel_comp / "firmware-armv7").write_bytes(b"gw")
            (rel_comp / "web/index.html").write_text("<html></html>")
            (rel_comp / "model/silero_vad.onnx").write_bytes(b"vad")

            kws = root / "kws-bundle"
            kws.mkdir()
            (kws / "hm-kws").write_bytes(b"kws")
            (kws / "run.sh").write_text("#!/bin/sh\nexec ./hm-kws\n")

            sup = root / "start_server.sh"
            sup.write_text("#!/bin/sh\n")
            core_sup = root / "hm-core-supervisor.sh"
            core_sup.write_text("#!/bin/sh\n")
            assets_empty = root / "assets-empty"
            assets_empty.mkdir()

            manifest = {"release": {"version": "test"}, "target": {}, "components": []}

            def make_args(assets_dir: Path, allow_empty: bool):
                return types.SimpleNamespace(
                    device_id="dot2-test",
                    core_supervisor=str(core_sup),
                    supervisor=str(sup),
                    installer="",
                    assets_dir=str(assets_dir),
                    allow_empty_wake_acks=allow_empty,
                    kws_bundle=str(kws),
                )

            # 1. Default strict mode MUST exit(1) when wake-acks/manifest.json is missing.
            with self.assertRaises(SystemExit) as ctx:
                mod.stage_tree(root / "seed-strict", make_args(assets_empty, False), rel_comp, manifest)
            self.assertEqual(ctx.exception.code, 1)

            # 2. --allow-empty-wake-acks stages cleanly without wake-acks directory.
            seed_ok = root / "seed-allow-empty"
            meta = mod.stage_tree(seed_ok, make_args(assets_empty, True), rel_comp, manifest)
            self.assertEqual(meta["release"]["version"], "test")
            self.assertTrue((seed_ok / "hm/bin/core_a").is_file())
            self.assertTrue((seed_ok / "install.sh").is_file())
            self.assertEqual((seed_ok / "hm-direct/device-id").read_text().strip(), "dot2-test")
            self.assertFalse((seed_ok / "hm/data/assets/wake-acks").exists())

            # 3. When wake-acks/manifest.json exists, it is copied into the seed normally.
            assets_present = root / "assets-present"
            (assets_present / "wake-acks").mkdir(parents=True)
            (assets_present / "wake-acks/manifest.json").write_text('{"acks":[]}\n')
            seed_acks = root / "seed-with-acks"
            mod.stage_tree(seed_acks, make_args(assets_present, False), rel_comp, manifest)
            self.assertTrue((seed_acks / "hm/data/assets/wake-acks/manifest.json").is_file())


if __name__ == "__main__":
    unittest.main()
