#!/usr/bin/env python3
"""Unit tests for the Hermes L1 home-mate-fleet skill CLI (#267)."""

from __future__ import annotations

import http.server
import io
import json
import os
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from typing import Any

import fleet_cli


class _MockSpeakerHandler(http.server.BaseHTTPRequestHandler):
    received_announces: list[dict[str, Any]] = []
    expected_token: str = "fleet-test-token"

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        return

    def _check_auth(self) -> bool:
        if self.headers.get("Authorization") != f"Bearer {self.expected_token}":
            self.send_response(401)
            self.end_headers()
            self.wfile.write(b'{"error":"unauthorized"}')
            return False
        return True

    def _send_json(self, code: int, body: dict[str, Any]) -> None:
        raw = json.dumps(body, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:  # noqa: N802
        if not self._check_auth():
            return
        if self.path == "/api/v0/status":
            self._send_json(200, {"ok": True, "version": "0.1.0"})
        elif self.path == "/api/v0/mode":
            # Exact hm-core /api/v0/mode schema: uses `device` (not `id`/`online`).
            self._send_json(
                200,
                {
                    "devices": [
                        {
                            "device": "dot2-d",
                            "activity": "idle",
                            "muted": False,
                            "kws": {"state": "loaded", "detections": 3},
                        }
                    ]
                },
            )
        elif self.path == "/api/v0/devices":
            # Exact hm-core /api/v0/devices schema: `gateway.devices[].stable_id` + `online`.
            self._send_json(
                200,
                {
                    "gateway": {
                        "devices": [
                            {"stable_id": "dot2-d", "online": True},
                        ]
                    }
                },
            )
        elif self.path.startswith("/api/v0/voice/events"):
            self._send_json(200, {"events": [{"kind": "voice_announce_spoken", "device": "dot2-d"}]})
        else:
            self._send_json(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if not self._check_auth():
            return
        if self.path == "/api/v0/voice/announce":
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length).decode("utf-8"))
            self.__class__.received_announces.append(body)
            self._send_json(202, {"id": "A-1", "status": "accepted"})
        else:
            self._send_json(404, {"error": "not found"})


class FleetCliTest(unittest.TestCase):
    def setUp(self) -> None:
        _MockSpeakerHandler.received_announces = []
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _MockSpeakerHandler)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

        self.tmpdir = tempfile.TemporaryDirectory()
        self.config_path = Path(self.tmpdir.name) / "fleet.json"
        self.config_path.write_text(
            json.dumps(
                {
                    "nodes": {
                        "study": {
                            "url": f"http://127.0.0.1:{self.port}",
                            "token": "fleet-test-token",
                            "room": "书房",
                        }
                    }
                },
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.tmpdir.cleanup()

    def _run_cli(self, argv: list[str]) -> tuple[int, dict[str, Any]]:
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = fleet_cli.main(["--config", str(self.config_path), *argv])
        return rc, json.loads(buf.getvalue())

    def test_fleet_merges_mode_and_devices_online_state(self) -> None:
        rc, out = self._run_cli(["fleet"])
        self.assertEqual(rc, 0)
        study = out["nodes"]["study"]
        self.assertTrue(study["reachable"])
        self.assertEqual(study["room"], "书房")
        self.assertEqual(len(study["devices"]), 1)
        dev = study["devices"][0]
        self.assertEqual(dev["id"], "dot2-d")
        self.assertIs(dev["online"], True)
        self.assertEqual(dev["activity"], "idle")
        self.assertEqual(dev["kws_state"], "loaded")

    def test_events_and_announce(self) -> None:
        rc, ev = self._run_cli(["events", "study", "--limit", "10"])
        self.assertEqual(rc, 0)
        self.assertTrue(ev["ok"])
        self.assertEqual(ev["data"]["events"][0]["kind"], "voice_announce_spoken")

        rc, ann = self._run_cli(
            [
                "announce",
                "study",
                "--text",
                "书房音箱状态正常。",
                "--device",
                "dot2-d",
                "--task-id",
                "run_99",
                "--priority",
                "high",
                "--open-followup",
            ]
        )
        self.assertEqual(rc, 0)
        self.assertTrue(ann["ok"])
        self.assertEqual(len(_MockSpeakerHandler.received_announces), 1)
        sent = _MockSpeakerHandler.received_announces[0]
        self.assertEqual(sent["text"], "书房音箱状态正常。")
        self.assertEqual(sent["device"], "dot2-d")
        self.assertEqual(sent["task_id"], "run_99")
        self.assertEqual(sent["priority"], "high")
        self.assertTrue(sent["open_followup"])
        self.assertEqual(sent["source"], "hermes_fleet_skill")


if __name__ == "__main__":
    unittest.main()
