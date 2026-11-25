#!/usr/bin/env python3
"""Read-only fleet inspection and controlled voice announcement helper for Hermes (#267 Slice 3).

Reads node inventory from $HOME_MATE_FLEET_CONFIG (JSON):
{
  "nodes": {
    "living-room": { "url": "http://192.168.1.21:8930", "token": "...", "room": "客厅" }
  }
}

Safety boundary (L1 Fleet Self-Diagnosis):
- Inspection commands (`fleet`, `status`, `mode`, `events`, `diagnostics`) are strictly GET / read-only.
- `announce` calls only `POST /api/v0/voice/announce` (non-interrupting bounded queue).
- Never mutates speaker config, never restarts services, never flashes firmware.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

MAX_BODY_BYTES = 512 * 1024


def load_nodes(config_path: str | None) -> dict[str, dict[str, Any]]:
    path = config_path or os.environ.get("HOME_MATE_FLEET_CONFIG", "")
    if not path:
        raise RuntimeError("HOME_MATE_FLEET_CONFIG is not set and --config was not provided")
    p = Path(path)
    if not p.is_file():
        raise RuntimeError(f"fleet config file not found: {p}")
    raw = json.loads(p.read_text(encoding="utf-8"))
    nodes = raw.get("nodes", raw) if isinstance(raw, dict) else {}
    if not isinstance(nodes, dict) or not nodes:
        raise RuntimeError("fleet config contains no nodes")
    return {str(k): v for k, v in nodes.items() if isinstance(v, dict)}


def call_node(
    node: dict[str, Any],
    method: str,
    path: str,
    payload: dict[str, Any] | None = None,
    timeout: float = 6.0,
) -> dict[str, Any]:
    url = f"{str(node['url']).rstrip('/')}{path}"
    headers = {"Accept": "application/json"}
    token = str(node.get("token") or "").strip()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = None
    if payload is not None:
        data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        headers["Content-Type"] = "application/json; charset=utf-8"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read(MAX_BODY_BYTES)
            return {
                "ok": True,
                "http": resp.status,
                "data": json.loads(raw.decode("utf-8")) if raw else {},
            }
    except urllib.error.HTTPError as err:
        raw = err.read(MAX_BODY_BYTES)
        return {
            "ok": False,
            "http": err.code,
            "error": raw.decode("utf-8", errors="replace")[:300],
        }
    except Exception as exc:
        return {"ok": False, "http": 0, "error": str(exc)}


def cmd_fleet(nodes: dict[str, dict[str, Any]]) -> dict[str, Any]:
    summary: dict[str, Any] = {}
    for name, node in nodes.items():
        st = call_node(node, "GET", "/api/v0/status", timeout=4.0)
        mode = call_node(node, "GET", "/api/v0/mode", timeout=4.0) if st["ok"] else None
        devs_resp = call_node(node, "GET", "/api/v0/devices", timeout=4.0) if st["ok"] else None
        online_map: dict[str, bool] = {}
        if devs_resp and devs_resp["ok"]:
            gw_devs = (devs_resp["data"].get("gateway") or {}).get("devices")
            if not isinstance(gw_devs, list):
                gw_devs = devs_resp["data"].get("devices") or []
            for gd in gw_devs:
                if isinstance(gd, dict):
                    sid = gd.get("stable_id") or gd.get("device") or gd.get("id")
                    if sid:
                        online_map[str(sid)] = bool(gd.get("online", False))
        devs = []
        seen: set[str] = set()
        if mode and mode["ok"]:
            for d in mode["data"].get("devices", []):
                if not isinstance(d, dict):
                    continue
                dev_id = d.get("device") or d.get("stable_id") or d.get("id")
                dev_key = str(dev_id) if dev_id else ""
                if dev_key:
                    seen.add(dev_key)
                online = online_map[dev_key] if dev_key in online_map else d.get("online")
                devs.append(
                    {
                        "id": dev_id,
                        "online": online,
                        "activity": d.get("activity"),
                        "muted": d.get("muted"),
                        "kws_state": (d.get("kws") or {}).get("state"),
                        "kws_detections": (d.get("kws") or {}).get("detections"),
                    }
                )
        for sid, is_online in online_map.items():
            if sid not in seen:
                devs.append({"id": sid, "online": is_online})
        summary[name] = {
            "room": node.get("room"),
            "reachable": st["ok"],
            "http": st["http"],
            "error": st.get("error"),
            "devices": devs,
        }
    return {"nodes": summary}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Home Mate Fleet L1 Diagnostics & Announce CLI")
    parser.add_argument("--config", help="Path to fleet.json (defaults to $HOME_MATE_FLEET_CONFIG)")
    sub = parser.add_subparsers(dest="cmd", required=True)

    sub.add_parser("fleet", help="Read-only health summary of every speaker in the household fleet")

    for name in ("status", "mode", "diagnostics"):
        p = sub.add_parser(name, help=f"GET /api/v0/{name} on one speaker node")
        p.add_argument("node", help="Node key from fleet.json")

    p_ev = sub.add_parser("events", help="GET recent voice journal events from one speaker node")
    p_ev.add_argument("node", help="Node key from fleet.json")
    p_ev.add_argument("--limit", type=int, default=40, help="Max events to fetch (default 40)")

    p_ann = sub.add_parser("announce", help="Send a controlled announcement via POST /api/v0/voice/announce")
    p_ann.add_argument("node", help="Node key from fleet.json")
    p_ann.add_argument("--text", required=True, help="Concise spoken text (<= 500 chars)")
    p_ann.add_argument("--device", default="", help="Optional device stable_id")
    p_ann.add_argument("--task-id", default="", help="Optional task correlation ID")
    p_ann.add_argument("--priority", choices=("normal", "high"), default="normal")
    p_ann.add_argument("--open-followup", action="store_true")

    args = parser.parse_args(argv)
    try:
        nodes = load_nodes(args.config)
        if args.cmd == "fleet":
            out = cmd_fleet(nodes)
        else:
            if args.node not in nodes:
                raise RuntimeError(f"unknown node '{args.node}' (known: {', '.join(sorted(nodes))})")
            node = nodes[args.node]
            if args.cmd in ("status", "mode", "diagnostics"):
                out = call_node(node, "GET", f"/api/v0/{args.cmd}")
            elif args.cmd == "events":
                limit = max(1, min(args.limit, 200))
                out = call_node(node, "GET", f"/api/v0/voice/events?tail=true&limit={limit}")
            elif args.cmd == "announce":
                payload: dict[str, Any] = {
                    "text": args.text,
                    "priority": args.priority,
                    "open_followup": bool(args.open_followup),
                    "source": "hermes_fleet_skill",
                }
                if args.device:
                    payload["device"] = args.device
                if args.task_id:
                    payload["task_id"] = args.task_id
                out = call_node(node, "POST", "/api/v0/voice/announce", payload=payload)
            else:
                raise RuntimeError(f"unsupported command: {args.cmd}")
        print(json.dumps(out, ensure_ascii=False, indent=2))
        return 0
    except Exception as exc:
        print(json.dumps({"ok": False, "error": str(exc)}, ensure_ascii=False), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
