#!/usr/bin/env python3
"""Replay additive DeviceStatus diagnostics against actual old/new Core mains.

Requires independently built binaries; records their SHA-256 and isolated
process outcomes. This is decoder/API compatibility evidence, not Firmware,
audio, distributed ownership or release acceptance.
"""

import argparse
import hashlib
import json
from pathlib import Path
import socket
import tempfile
import time

from system_smoke import Core, SmokeFailure, require, reserve_ports


class Peer:
    def __init__(self, port):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=3)
        self.socket.settimeout(3)
        self.reader = self.socket.makefile("rb")
        # Old-style Hello intentionally omits min_protocol/gateway_version.
        self.send({"type": "hello", "protocol": 0, "gateway": "compat-fixture",
                   "devices": [{"stable_id": "fixture-dot", "model": "fixture",
                                "capabilities": [], "audio_profile": "fixture"}]})
        line = self.reader.readline(1_000_001)
        require(len(line) <= 1_000_000 and line.endswith(b"\n"), "unbounded/missing handshake")
        ack = json.loads(line)
        require(ack.get("type") == "hello_ack" and ack.get("protocol") == 0,
                "actual old/new binary must negotiate wire0")

    def send(self, value):
        self.socket.sendall(json.dumps(value, separators=(",", ":")).encode() + b"\n")

    def close(self):
        try:
            self.socket.shutdown(socket.SHUT_WR)
        except OSError:
            pass
        self.reader.close()
        self.socket.close()


def until(core, path, predicate):
    deadline = min(core.deadline, time.monotonic() + 5)
    while time.monotonic() < deadline:
        value = core.request("GET", path)
        if predicate(value):
            return value
        time.sleep(0.01)
    raise SmokeFailure(f"{core.name}: compatibility readback did not converge on {path}")


def device(value):
    return next((d for d in value["gateway"]["devices"] if d["stable_id"] == "fixture-dot"), {})


def status_seen(value, reason):
    return any(e.get("kind") == "device_status" and e.get("reason") == reason
               for e in value.get("recent", []))


def run(args):
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="compat-", dir=args.output_dir.resolve()))
    fixture_path = Path(__file__).resolve().parents[1] / "docs/fixtures/northbound-status-v1.json"
    fixture = json.loads(fixture_path.read_bytes())
    deadline = time.monotonic() + args.timeout
    ports = reserve_ports(4)
    cores = []
    peers = []
    report = {"status": "failed", "scope": "actual-old-new-core-wire0-additive-diagnostics",
              "fixture_sha256": hashlib.sha256(fixture_path.read_bytes()).hexdigest(), "checks": [], "binaries": {}}
    try:
        for index, (label, binary) in enumerate((("old", args.old_core), ("new", args.new_core))):
            binary = binary.resolve()
            require(binary.is_file(), f"missing {label} Core binary")
            report["binaries"][label] = {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
            core = Core(binary, root, f"compat-{label}", ports[index * 2], ports[index * 2 + 1], deadline)
            cores.append(core)
            core.start()
            core.token = core.request("POST", "/api/v0/auth/provision")["token"]
            peer = Peer(core.gateway_port)
            peers.append(peer)
            peer.send(fixture)
            observed = until(core, "/api/v0/devices", lambda v: status_seen(v, "registered"))
            require(observed["gateway"]["protocol_operating"] == 0, "diagnostic extension changed wire negotiation")
            if label == "old":
                require("northbound" not in device(observed), "old binary unexpectedly exposes new schema")
                # The old handler really consumed the enclosing status, rather
                # than merely keeping its pre-existing Hello device online.
                offline = dict(fixture, online=False, reason="compat-old-offline")
                peer.send(offline)
                until(core, "/api/v0/devices", lambda v: status_seen(v, "compat-old-offline") and device(v).get("online") is False)
                unsupported = dict(fixture, online=True, reason="compat-old-unknown", northbound={"schema": 999})
                peer.send(unsupported)
                until(core, "/api/v0/devices", lambda v: status_seen(v, "compat-old-unknown") and device(v).get("online") is True)
                report["checks"].append("new schema1/unknown-schema DeviceStatus consumed by actual old wire0 binary; old API remains legacy")
            else:
                require(device(observed)["northbound"]["sample"] == fixture["northbound"], "new binary failed actual schema1 readback")
                legacy = {k: v for k, v in fixture.items() if k != "northbound"}
                legacy["reason"] = "compat-new-legacy"
                peer.send(legacy)
                until(core, "/api/v0/devices", lambda v: status_seen(v, "compat-new-legacy") and device(v)["northbound"]["state"] == "absent")
                unsupported = dict(fixture, reason="compat-new-unknown", northbound={"schema": 999, "untrusted": "not-retained"})
                peer.send(unsupported)
                unknown = until(core, "/api/v0/diagnostics", lambda v: device(v)["northbound"]["state"] == "unsupported")
                require(device(unknown)["northbound"]["sample"] is None and "not-retained" not in json.dumps(unknown), "unknown schema payload retained")
                malformed = json.loads(json.dumps(fixture))
                del malformed["northbound"]["critical_current_bytes"]
                peer.send(malformed)
                invalid = until(core, "/api/v0/diagnostics", lambda v: device(v)["northbound"]["state"] == "invalid")
                require(invalid["gateway"]["connected"] and device(invalid)["northbound"]["sample"] is None,
                        "malformed optional diagnostics must remain unavailable without link loss")
                peer.send(fixture)
                until(core, "/api/v0/diagnostics", lambda v: device(v)["northbound"]["state"] == "available")
                peer.close()
                peers.remove(peer)
                until(core, "/api/v0/diagnostics", lambda v: v["gateway"]["connected"] is False and device(v)["northbound"]["stale"] is True)
                peer = Peer(core.gateway_port)
                peers.append(peer)
                require(device(core.request("GET", "/api/v0/diagnostics"))["northbound"]["stale"] is True, "Hello alone refreshed a stale report")
                peer.send(legacy)
                until(core, "/api/v0/diagnostics", lambda v: device(v)["northbound"]["state"] == "absent")
                report["checks"].append("actual new binary consumes old status as absent, unknown schema unavailable, disconnect/reconnect remains stale until a report")
        report["status"] = "passed"
    except Exception as exc:
        report["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        for peer in peers:
            peer.close()
        report["nodes"] = []
        for core in cores:
            clean = core.stop()
            report["nodes"].append({"name": core.name, "pid": core.proc.pid if core.proc else None,
                                    "data_dir": str(core.data), "exit_code": core.proc.returncode if core.proc else None})
            if not clean:
                report["status"] = "failed"
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "report": str(root / "report.json")}))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--old-core", type=Path, required=True)
    parser.add_argument("--new-core", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=45)
    raise SystemExit(run(parser.parse_args()))
