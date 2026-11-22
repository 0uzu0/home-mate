#!/usr/bin/env python3
"""Compute per-unit `mic_gain_offset_db` from recent `/api/v0/voice/events`.

Reads `wake_claim` and `arbitration_*` events from each reachable Dot2 unit,
extracts `rms_dbfs` samples, computes per-unit median dBFS, and suggests
`mic_gain_offset_db = baseline_median - unit_median` relative to a chosen
baseline unit.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
import urllib.error
import urllib.request
from typing import Any


def parse_unit_spec(spec: str, default_port: int, default_token: str | None) -> tuple[str, str, str | None]:
    """Parse `id=host[:port][@token]` into `(unit_id, base_url, token)`."""
    if "=" not in spec:
        raise ValueError(f"invalid --unit {spec!r}: expected id=host[:port][@token]")
    unit_id, rest = spec.split("=", 1)
    token = default_token
    if "@" in rest:
        rest, token = rest.rsplit("@", 1)
    if rest.startswith("http://") or rest.startswith("https://"):
        base_url = rest.rstrip("/")
    elif ":" in rest:
        base_url = f"http://{rest}"
    else:
        base_url = f"http://{rest}:{default_port}"
    return unit_id.strip(), base_url, token


def fetch_events(base_url: str, token: str | None, limit: int, timeout: float) -> list[dict[str, Any]]:
    url = f"{base_url}/api/v0/voice/events?limit={limit}"
    req = urllib.request.Request(url)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = json.loads(resp.read().decode("utf-8"))
    events = payload.get("events", [])
    return events if isinstance(events, list) else []


def extract_rms_samples(events: list[dict[str, Any]], unit_id: str) -> list[float]:
    """Extract `rms_dbfs` from `wake_claim` and `arbitration_*` events for `unit_id`."""
    samples: list[float] = []
    for ev in events:
        if not isinstance(ev, dict):
            continue
        kind = str(ev.get("kind", ""))
        if kind != "wake_claim" and not kind.startswith("arbitration_"):
            continue
        # Direct `rms_dbfs` on the event (when device matches or is omitted).
        ev_dev = ev.get("device")
        if (ev_dev is None or ev_dev == unit_id) and isinstance(ev.get("rms_dbfs"), (int, float)):
            samples.append(float(ev["rms_dbfs"]))
            continue
        # Nested `claim.rms_dbfs` if present.
        claim = ev.get("claim")
        if isinstance(claim, dict) and claim.get("device") == unit_id:
            if isinstance(claim.get("rms_dbfs"), (int, float)):
                samples.append(float(claim["rms_dbfs"]))
                continue
        # Nested `decision.claims` list if present.
        dec = ev.get("decision")
        if isinstance(dec, dict) and isinstance(dec.get("claims"), list):
            for c in dec["claims"]:
                if isinstance(c, dict) and c.get("device") == unit_id and isinstance(c.get("rms_dbfs"), (int, float)):
                    samples.append(float(c["rms_dbfs"]))
    return samples


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Compute per-unit mic_gain_offset_db from voice events.")
    ap.add_argument(
        "--unit",
        action="append",
        required=True,
        help="Unit spec: id=host[:port][@token] (repeatable)",
    )
    ap.add_argument("--baseline", required=True, help="Baseline unit id (offset 0.0 dB)")
    ap.add_argument("--port", type=int, default=8930, help="Default core HTTP port (default: 8930)")
    ap.add_argument("--token", default=None, help="Default bearer token")
    ap.add_argument("--limit", type=int, default=500, help="Max events to fetch per unit (default: 500)")
    ap.add_argument("--timeout", type=float, default=4.0, help="HTTP timeout in seconds (default: 4.0)")
    args = ap.parse_args(argv)

    medians: dict[str, float] = {}
    counts: dict[str, int] = {}
    errors: dict[str, str] = {}

    for spec in args.unit:
        unit_id, base_url, token = parse_unit_spec(spec, args.port, args.token)
        try:
            events = fetch_events(base_url, token, args.limit, args.timeout)
        except (urllib.error.URLError, TimeoutError, ValueError) as exc:
            errors[unit_id] = str(exc)
            continue
        samples = extract_rms_samples(events, unit_id)
        counts[unit_id] = len(samples)
        if samples:
            medians[unit_id] = statistics.median(samples)

    if args.baseline not in medians:
        print(
            f"error: baseline unit {args.baseline!r} has no rms_dbfs samples "
            f"(errors={errors})",
            file=sys.stderr,
        )
        return 1

    baseline_db = medians[args.baseline]
    print(f"Baseline: {args.baseline} = {baseline_db:.2f} dBFS (n={counts[args.baseline]})")
    print(f"{'Unit':<12} {'Samples':>7} {'Median dBFS':>12} {'Offset dB':>10}  Payload")
    print("-" * 78)
    for spec in args.unit:
        unit_id, _, _ = parse_unit_spec(spec, args.port, args.token)
        if unit_id in errors:
            print(f"{unit_id:<12} {'ERR':>7} {'-':>12} {'-':>10}  {errors[unit_id]}")
            continue
        n = counts.get(unit_id, 0)
        if unit_id not in medians:
            print(f"{unit_id:<12} {n:>7} {'-':>12} {'-':>10}  no rms_dbfs samples")
            continue
        med = medians[unit_id]
        offset = round(baseline_db - med, 2)
        payload = json.dumps({"mic_gain_offset_db": offset, "mic_calibrated": True})
        print(f"{unit_id:<12} {n:>7} {med:>12.2f} {offset:>+10.2f}  {payload}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
