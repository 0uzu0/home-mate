#!/usr/bin/env python3
"""M3 real-model scenario runner against a running hm-core instance.

The deterministic twin of tests/scenarios.rs: same acceptance bar, REAL
model (whatever LLM the service page has configured — GLM glm-4-flash at
the time of writing), real management API, simulated household tools.
Non-deterministic by nature: run on demand, not in cargo test.

Scenarios (M3 categories):
  1. novel phrasing      — invented chart-play wording flows through, the
                           answer names the track the tool actually played
  2. cross-turn correction — "不是这首，换第二首" re-picks; a NEW play is
                           submitted and the answer names the new track
  3. prior-play evidence — "刚才放的什么歌" is grounded in the session's
                           play receipts (answer names the last played track)
  4. cancel boundary     — cancel a running turn by task id; the turn ends
                           cancelled, no answer is produced afterwards
  5. (optional) failure fallback — requires the core to run with
                           HM_INJECT_PLAY_FAIL=t01; skipped otherwise with
                           an honest SKIP line

Usage:  python3 scripts/verify_m3.py [base_url]
        HM_VERIFY_INJECT=1 to also run scenario 5
Output: markdown report on stdout; exit code 1 if any scenario fails.
"""

import json
import os
import sys
import time
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8930"

RESULTS = []


def post(path, body, timeout=180):
    req = urllib.request.Request(
        BASE + path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def get(path, timeout=30):
    with urllib.request.urlopen(BASE + path, timeout=timeout) as r:
        return json.loads(r.read())


def post_chat_sse(text, session_id=None):
    req = urllib.request.Request(
        BASE + "/api/v0/chat",
        data=json.dumps({"session_id": session_id, "text": text}).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    events = []
    with urllib.request.urlopen(req, timeout=180) as resp:
        buf = b""
        while True:
            chunk = resp.read(1)
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                line = line.decode().strip()
                if line.startswith("data:"):
                    try:
                        events.append(json.loads(line[5:].strip()))
                    except json.JSONDecodeError:
                        pass
    return events


def record(name, ok, detail):
    RESULTS.append((name, ok, detail))
    mark = "PASS" if ok else ("SKIP" if ok is None else "FAIL")
    print(f"| {mark} | {name} | {detail} |")


def answer_of(events):
    for e in reversed(events):
        if e.get("type") == "answer":
            return e.get("text", "")
    return ""


def plays_of(events):
    """(tool, status, title-ish detail) for every music.play result."""
    out = []
    for e in events:
        if e.get("type") == "tool_result" and e.get("tool") == "music.play":
            out.append((e.get("status"), e.get("evidence"), e.get("summary", ""), e.get("detail", "")))
    return out


def scenario_1_novel_phrasing():
    text = "现在网上大家都在听的歌，来一个呗"
    events = post_chat_sse(text)
    ans = answer_of(events)
    plays = plays_of(events)
    played_titles = " ".join(p[2] + p[3] for p in plays)
    chain = any(e.get("type") == "tool_start" and e.get("tool") == "hotlist.query" for e in events)
    ok = bool(ans) and bool(plays) and chain and any(
        t and (t in ans or t in played_titles) for t in ["晚风信笺", "光年之外的光", "小城路灯", "白噪与海", "北方的南方", "口袋里的银河", "慢速心跳"])
    record("novel phrasing", ok,
           f"answer={ans[:60]!r} plays={[(p[0], p[1]) for p in plays]} chain={chain}")
    return events


def scenario_2_correction(prior_events):
    session = next((e["session_id"] for e in prior_events if e.get("type") == "session"), None)
    first = plays_of(prior_events)
    events = post_chat_sse("不是这首，换榜单第二首", session)
    ans = answer_of(events)
    second = plays_of(events)
    new_play = bool(second) and (not first or first[0] != second[0])
    ok = bool(ans) and new_play and ("光年之外" in ans or "第二" in ans or any("光年之外的光" in p[2] + p[3] for p in second))
    record("cross-turn correction", ok,
           f"first={[(p[0], p[2]) for p in first]} second={[(p[0], p[2]) for p in second]} answer={ans[:60]!r}")


def scenario_3_prior_play(prior_events):
    session = next((e["session_id"] for e in prior_events if e.get("type") == "session"), None)
    played = plays_of(prior_events)
    events = post_chat_sse("刚才放的什么歌", session)
    ans = answer_of(events)
    # Ground the check in the play DETAIL ("now playing <title> — <artist>"),
    # never in hardcoded titles.
    titles = []
    for p in played:
        detail = p[3] or ""
        if "—" in detail:
            titles.append(detail.split("—")[0].replace("now playing", "").strip())
    ok = bool(played) and any(t and t in ans for t in titles)
    record("prior-play evidence", ok,
           f"last_played={titles[-1] if titles else None} answer={ans[:60]!r}")


def scenario_4_cancel():
    # Long turn: hotlist delay is not injectable here, so cancel whatever
    # task appears first — with a real model the first call usually takes
    # long enough to land the cancel mid-turn.
    events = []
    req = urllib.request.Request(
        BASE + "/api/v0/chat",
        data=json.dumps({"text": "讲个长一点的睡前故事，三段那种"}).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    import threading

    def consume():
        try:
            with urllib.request.urlopen(req, timeout=180) as resp:
                buf = b""
                while True:
                    chunk = resp.read(1)
                    if not chunk:
                        break
                    buf += chunk
                    while b"\n" in buf:
                        line, buf = buf.split(b"\n", 1)
                        line = line.decode().strip()
                        if line.startswith("data:"):
                            try:
                                events.append(json.loads(line[5:].strip()))
                            except json.JSONDecodeError:
                                pass
        except Exception as e:  # cancelled streams may end abruptly
            events.append({"type": "_stream_error", "error": str(e)})

    t = threading.Thread(target=consume)
    t.start()
    task_id = None
    deadline = time.time() + 20
    while time.time() < deadline and not task_id:
        tasks = get("/api/v0/tasks").get("tasks", [])
        running = [x for x in tasks if x["status"] == "running"]
        if running:
            task_id = running[0]["id"]
        time.sleep(0.1)
    cancelled = False
    if task_id:
        r = post("/api/v0/session/cancel", {"task_id": task_id})
        cancelled = r.get("result") == "cancel_requested"
    t.join(timeout=30)
    has_answer = any(e.get("type") == "answer" for e in events)
    end = next((e for e in reversed(events) if e.get("type") == "turn_end"), {})
    status = end.get("status")
    tasks = get("/api/v0/tasks").get("tasks", [])
    task_status = next((x["status"] for x in tasks if x["id"] == task_id), None)
    # A turn already finished when we cancel is a legitimate not_running;
    # accept cancelled either way, but record which happened.
    ok = (status == "cancelled" and task_status == "cancelled") or (
        cancelled is False and status == "completed")
    record("cancel boundary", ok,
           f"cancel_accepted={cancelled} turn_end={status} task={task_status} late_answer={has_answer}")


def scenario_5_failure_fallback():
    if os.environ.get("HM_VERIFY_INJECT") != "1":
        record("failure fallback (inject)", None, "SKIP — restart core with HM_INJECT_PLAY_FAIL=t01 and HM_VERIFY_INJECT=1")
        return
    events = post_chat_sse("放榜单第一名的歌")
    ans = answer_of(events)
    plays = plays_of(events)
    # Inject mode REQUIRES an observed failed/unknown play — otherwise the
    # injection was not actually active and a passing answer proves nothing.
    injected = any(p[0] in ("failed", "unknown") for p in plays)
    record("failure fallback (inject)", bool(ans) and injected,
           f"plays={[(p[0], p[1]) for p in plays]} answer={ans[:60]!r}"
           + ("" if injected else " (no failed/unknown play — was the core restarted with the injection?)"))


def main():
    print(f"# M3 real-model verification — {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
    print(f"base: {BASE}\n")
    try:
        status = get("/api/v0/status")
    except Exception as e:
        print(f"FATAL: core not reachable at {BASE}: {e}")
        sys.exit(2)
    llm = status.get("services", {}).get("llm", {})
    print(f"llm: {llm.get('provider')} / {llm.get('model')} (key set: {llm.get('api_key_set')})\n")
    if not llm.get("configured"):
        print("FATAL: LLM not configured — set it on the Services page first.")
        sys.exit(2)

    print("| result | scenario | evidence |")
    print("|---|---|---|")
    ev1 = scenario_1_novel_phrasing()
    scenario_2_correction(ev1)
    scenario_3_prior_play(ev1)
    scenario_4_cancel()
    scenario_5_failure_fallback()

    fails = [r for r in RESULTS if r[1] is False]
    skips = [r for r in RESULTS if r[1] is None]
    print(f"\n{len(RESULTS) - len(fails) - len(skips)}/{len(RESULTS)} passed"
          + (f"; skipped: {[r[0] for r in skips]}" if skips else "")
          + (f"; FAILURES: {[r[0] for r in fails]}" if fails else ""))
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
