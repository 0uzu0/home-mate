#!/usr/bin/env python3
"""R1g/R1h interaction-matrix acceptance on the real device.

Every check drives ONE cell of the matrix through the same entrances a person
uses (the 2K USB camera speaker for speech, the management API for playback
and inspection) and prints PASS/FAIL with the numbers behind it. The policy
itself is unit/integration tested in the repo; this tool answers "does the
running core+firmware behave that way on this box".

Cells (docs/d/r1g-20261113.md §6):
  kws-idle     KWS with NOTHING playing  -> wake: window opens, no stop
  kws-media    KWS during MEDIA playback -> the media play stops
  unwoken-story story playing + a command WITHOUT the wake word -> nothing stops
  unwoken-media music playing + a command WITHOUT the wake word -> music keeps
                playing and the transcript is counted as unwoken
  unwoken-idle  nothing playing + a command WITHOUT the wake word -> no turn

The `unwoken-*` checks are NEGATIVE tests: the command is spoken WITHOUT the
wake word on purpose, and the pass condition is "the device did nothing".
Triggering them with a wake word would be a different cell (kws-*).
  voice-pause-resume-retreat
               story -> 你好高达 (stops + bookmark) -> 继续 -> 你好高达 -> 退下

R1h additions (same device, same entrances — a registered service must obey
the SAME matrix, and a truncated program must be labelled):
  weather-now         问天气 -> weather.now 真的被调用且真的取到数 -> 播报
  weather-unconfigured 关掉天气 -> 工具如实报未配置/已关闭，且 0 次 HTTP
  weather-interrupt   天气播报中喊唤醒词 -> 停 + 书签 -> 继续
  truncation          长故事生成中被唤醒词打断 -> 继续 -> 播完前缀后报截断

Usage:
  tools/r1g_matrix.py [check ...]        # default: every check
  HM_CORE=http://192.168.1.73:8930 tools/r1g_matrix.py kws-idle

Stimuli live in data/acoustic/phrases (same ones the T4/T5 runs used);
`cmd_stop.wav` and `retreat.wav` are synthesized on the fly if missing.
"""
import json
import os
import subprocess
import sys
import threading
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PHRASES = os.path.join(HERE, "data/acoustic/phrases")
CAMERA_OUT = "2K USB Camera-Audio"
MAC_OUT = "Mac Studio扬声器"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
DEVICE = "dot2-d"
OUT = os.environ.get("HM_OUT", "/tmp/hm-r1g")
LONG_ASSET = "erle-stimulus.wav"  # 28 s: long enough to interrupt mid-way


# --- HTTP -------------------------------------------------------------------------

def get(path, timeout=8):
    with urllib.request.urlopen(CORE + path, timeout=timeout) as r:
        return json.load(r)


def post(path, body=None, timeout=60):
    req = urllib.request.Request(
        CORE + path, data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read()
    try:
        return json.loads(raw)
    except ValueError:
        # /api/v0/chat answers SSE, not JSON: the caller only needs the side
        # effect (the turn ran / the retreat was executed).
        return {"raw": raw[:120].decode(errors="replace")}


def put(path, body=None, timeout=20):
    req = urllib.request.Request(
        CORE + path, data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"}, method="PUT",
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def mode():
    """The whole mode snapshot. Settings that belong to the HOUSEHOLD (the
    truncation phrase, filler mode) live at the top level; per-device state
    lives under devices[0]."""
    return get("/api/v0/mode")


def dev():
    return mode()["devices"][0]


def weather_config():
    return get("/api/v0/config/services/weather")


def set_weather_enabled(enabled):
    cfg = weather_config()
    return put("/api/v0/config/services/weather", {
        "enabled": enabled,
        "provider": cfg.get("provider") or "open_meteo",
        "endpoint": cfg.get("endpoint") or "",
        "api_key_env": cfg.get("api_key_env") or "",
        "places": cfg.get("places") or [],
        "default_alias": cfg.get("default_alias") or "",
        "max_days": cfg.get("max_days") or 3,
    })


def last_turn_since(ts_ms):
    """(trace summary, trace detail, last assistant text) for the newest turn
    after `ts_ms` — the console's own view, nothing inferred. Device turns are
    recorded with source `device:asr` (the text path uses `text`)."""
    traces = get("/api/v0/trace?limit=10").get("traces") or []
    mine = [t for t in traces if t.get("created_ms", 0) >= ts_ms]
    if not mine:
        return None, None, ""
    # The list is newest-first; take the newest turn in the window.
    t = mine[0]
    detail = get(f"/api/v0/trace/{t['id']}")
    msgs = get(f"/api/v0/session/{t['session_id']}/messages").get("messages") or []
    answer = ""
    for m in msgs:
        if m.get("role") == "assistant":
            answer = m.get("content") or ""
    return t, detail, answer


def trace_ids():
    """Trace ids right now. Counting NEW ids after a command is clock-free:
    comparing the Mac's clock with the device's produced phantom counts before
    (the two boxes drift by seconds, recorded 2026-11-11)."""
    return {t["id"] for t in (get("/api/v0/trace?limit=30").get("traces") or [])}


def new_traces_since(ids):
    return [t for t in (get("/api/v0/trace?limit=30").get("traces") or []) if t["id"] not in ids]


def tool_events(detail):
    """Trace events with the payload flattened: [{kind, tool, status, error}]."""
    out = []
    for e in (detail or {}).get("events") or []:
        if e.get("kind") not in ("tool_start", "tool_result", "llm_end", "answer_rendered"):
            continue
        row = {"kind": e.get("kind")}
        row.update(e.get("payload") or {})
        out.append(row)
    return out


def say_continue_and_wait(t0, attempts=2, window=6.0):
    """Say 继续 and wait for the resume. The word is short enough that ASR
    sometimes hears something else (真机: 「继续。」→「法律。」), so a person's
    natural reaction — say it again — is modelled, and the number of spoken
    attempts is part of the evidence."""
    for i in range(attempts):
        say_it("continue_volc.wav")
        deadline = time.time() + window
        while time.time() < deadline:
            if any(e["kind"] == "program_resumed" for e in events_since(t0, {"program_resumed"})):
                return True, i + 1
            time.sleep(0.2)
    return False, attempts


def clear_device_session():
    """Start from an empty conversation: a model that answered the same
    question 30 s ago will otherwise reuse its own answer ("还是老样子")
    instead of consulting the tool again — true of any assistant, but it
    makes the tool-path check depend on history rather than on the service."""
    traces = get("/api/v0/trace?limit=1").get("traces") or []
    sid = (traces[0].get("session_id") if traces else None) or "dev:dot2-d"
    post(f"/api/v0/session/{sid}/clear", {})
    return sid


def wait_speech_done(timeout=40.0):
    """Wait for the answer to finish playing (activity back to idle/listening
    with no live outputs)."""
    deadline = time.time() + timeout
    started = False
    while time.time() < deadline:
        d = dev()
        if d.get("activity") in ("speaking", "thinking"):
            started = True
        elif started and not d.get("outputs"):
            return True
        time.sleep(0.2)
    return False


def events_since(ts_ms, kinds=None):
    evs = get("/api/v0/voice/log")["events"]
    out = [e for e in evs if e.get("ts_ms", 0) > ts_ms]
    if kinds:
        out = [e for e in out if e.get("kind") in kinds]
    return out


def now_ms():
    return int(time.time() * 1000)


# --- stimulus ---------------------------------------------------------------------

def phrase(name):
    # R1j: the PRODUCTION voice is the standard stimulus. The `_volc`
    # variants are synthesized with the product TTS; the older `wake_ni_gaoda`
    # recording is a different voice and the device spotter does not hear it
    # (2026-11-13: kws-idle failed on it while the same phrase in the `_volc`
    # voice fires). Prefer the production voice when both exist.
    volc = os.path.join(PHRASES, name.replace(".wav", "_volc.wav")) if "_volc" not in name else None
    if volc and os.path.exists(volc):
        return volc
    path = os.path.join(PHRASES, name)
    if os.path.exists(path):
        return path
    # No `say` fallback: macOS voices are not standard Mandarin and produced
    # a product-looking defect on 2026-11-12 ("停一下" -> "听一下"). Every
    # stimulus must come from the production voice:
    #   tools/tts_phrase.py "<text>" data/acoustic/phrases/<name>
    raise SystemExit(
        f"missing stimulus {name}: synthesize it with the production voice "
        f"(tools/tts_phrase.py \"<text>\" {path})"
    )


def say_it(name):
    """Play one stimulus through the camera speaker (the device's microphone
    hears the room, exactly like a person standing there)."""
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False, capture_output=True)
    try:
        subprocess.run(["afplay", phrase(name)], check=False)
    finally:
        subprocess.run([SWITCH, "-s", MAC_OUT], check=False, capture_output=True)


def play_asset(channel, asset=LONG_ASSET):
    """Start a diagnostic play WITHOUT waiting for it to finish.

    The endpoint answers after the device confirms the LAST frame (28 s), so
    it must run in a thread: the checks need the room while the audio is
    still playing.
    """
    threading.Thread(
        target=lambda: post(
            "/api/v0/diagnostics/tests/play_asset",
            {"asset": asset, "channel": channel},
            timeout=180,
        ),
        daemon=True,
    ).start()


def wait_activity(want, timeout=6.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if dev().get("activity") == want:
            return True
        time.sleep(0.1)
    return False


def settle(quiet_ms=1500, timeout=15.0):
    """Wait for a quiet device: nothing playing, no VAD segment open, and the
    mic has been quiet for `quiet_ms`. Back-to-back checks otherwise measure
    the previous stimulus (a wake phrase's tail, an ack, a media tail)."""
    deadline = time.time() + timeout
    quiet_since = None
    while time.time() < deadline:
        d = dev()
        busy = d.get("activity") not in ("idle", "listening") or d["mic"].get("in_speech")
        if busy:
            quiet_since = None
        else:
            quiet_since = quiet_since or time.time()
            if (time.time() - quiet_since) * 1000 >= quiet_ms:
                return True
        time.sleep(0.2)
    return False


def close_window(timeout=40.0):
    """The follow window is the wake word's 30 s lifetime. The negative checks
    need it CLOSED (an in-window command is a legitimate follow-up turn), so
    wait for the core's ticker to close it."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if dev()["session"]["state"] != "listening":
            return True
        time.sleep(1.0)
    return False


def wait_idle(timeout=12.0, close_window=False):
    """Stop the audio and wait for the device to be idle.

    `close_window=True` also waits out the 30 s follow window: a window left
    open by a previous wake makes an in-window transcript a legitimate
    follow-up turn, which would muddy the "no command without the wake word"
    assertion. The core closes the window itself on its presentation ticker.
    """
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    deadline = time.time() + timeout
    while time.time() < deadline:
        d = dev()
        # `outputs` must be empty too: a hub media play can outlive the stop
        # for a moment and would make the next stimulus land on the wrong
        # channel (a wake then stops the music instead of the story).
        if d.get("activity") in ("idle", "listening") and not d.get("outputs"):
            if not close_window or d["session"]["state"] != "listening":
                return True
        time.sleep(0.2)
    if close_window:
        # The window is derived from the wall clock (30 s); wait it out.
        deadline = time.time() + 40
        while time.time() < deadline:
            d = dev()
            if d.get("activity") in ("idle",) and d["session"]["state"] != "listening":
                return True
            time.sleep(1.0)
    return False


# --- checks -----------------------------------------------------------------------

results = []


def record(name, ok, detail):
    results.append((name, ok, detail))
    print(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")


def check_kws_idle():
    wait_idle()
    settle()
    before = dev()
    stops0 = before["kws"]["stops"]
    wakes0 = before["kws"].get("wakes", 0)
    t0 = now_ms()
    say_it("wake_ni_gaoda.wav")
    ok = False
    for _ in range(40):
        time.sleep(0.1)
        d = dev()
        if d["kws"].get("wakes", 0) > wakes0:
            ok = True
            break
    d = dev()
    evs = events_since(t0, {"keyword_wake", "keyword_stop", "keyword_ignored"})
    listening = d["session"]["state"] == "listening"
    stopped = d["kws"]["stops"] > stops0
    record(
        "kws-idle",
        ok and listening and not stopped,
        f"wakes +{d['kws'].get('wakes', 0) - wakes0} session={d['session']['state']} "
        f"stops +{d['kws']['stops'] - stops0} events={[e['kind'] for e in evs]}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    time.sleep(0.5)


def check_kws_media():
    wait_idle()
    settle()
    t0 = now_ms()
    play_asset("media")
    if not wait_activity("playing", 8):
        record("kws-media", False, f"premise failed: media never started ({dev().get('activity')})")
        return
    play_id = next((o["play_id"] for o in dev()["outputs"] if o.get("channel") == "media"), None)
    time.sleep(2.0)
    say_it("wake_ni_gaoda.wav")
    # Judge the SPECIFIC play: a hub media play leaves `outputs` when the
    # stop lands, and "activity" can flip for unrelated reasons.
    gone = False
    for _ in range(60):
        time.sleep(0.1)
        plays = [o["play_id"] for o in dev()["outputs"] if o.get("channel") == "media"]
        if play_id and play_id not in plays:
            gone = True
            break
    evs = events_since(t0, {"keyword_stop", "keyword_wake", "keyword_ignored", "wake_word_stop"})
    # Two entrances can stop media: the keyword fact, or the ASR wake when the
    # phrase is also transcribed (whichever lands first wins). The criterion is
    # that the media STOPPED through the wake word; the path is reported.
    by = next((e["kind"] for e in evs if e["kind"] in ("keyword_stop", "wake_word_stop")), None)
    kstop = [e for e in evs if e["kind"] == "keyword_stop"]
    latency = kstop[0].get("latency_ms") if kstop else None
    record(
        "kws-media",
        gone and bool(by),
        f"play={play_id} stopped={gone} stopped_by={by} keyword_latency_ms={latency} "
        f"events={[e['kind'] for e in evs]}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    time.sleep(0.5)


def check_no_cmd_story():
    wait_idle()
    settle()
    close_window()
    t0 = now_ms()
    play_asset("voice")
    if not wait_activity("speaking", 8):
        record("unwoken-story", False, f"premise failed: story never started ({dev().get('activity')})")
        return
    time.sleep(2.0)
    say_it("cmd_pause_volc.wav")
    time.sleep(4.0)
    d = dev()
    evs = events_since(t0, {"mic_utterance", "stop_intent", "session_retreat", "mic_ignored_unwoken",
                            "mic_suppressed_self_echo"})
    still = d.get("activity") == "speaking"
    acted = [e for e in evs if e["kind"] in ("mic_utterance", "stop_intent", "session_retreat")]
    record(
        "unwoken-story",
        still and not acted,
        f"still_speaking={still} acted={[e['kind'] for e in acted]} "
        f"events={[(e['kind'], e.get('text')) for e in evs]}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    time.sleep(0.5)


def check_no_cmd_media():
    wait_idle()
    settle()
    close_window()
    t0 = now_ms()
    play_asset("media")
    if not wait_activity("playing", 8):
        record("unwoken-media", False, f"premise failed: media never started ({dev().get('activity')})")
        return
    time.sleep(2.0)
    say_it("cmd_pause_volc.wav")
    # Poll instead of a fixed sleep: over MUSIC the VAD segment keeps growing
    # (the music is energy), so the transcript can arrive up to ~12 s later —
    # the core's 150-block segment cap. A fixed 4 s window misses it.
    ignored = []
    for _ in range(100):
        time.sleep(0.2)
        evs = events_since(t0, {"mic_utterance", "mic_ignored_unwoken", "stop_intent", "keyword_stop"})
        ignored = [e for e in evs if e["kind"] == "mic_ignored_unwoken"]
        if ignored:
            break
    d = dev()
    evs = events_since(t0, {"mic_utterance", "mic_ignored_unwoken", "stop_intent", "keyword_stop"})
    still = d.get("activity") == "playing"
    acted = [e for e in evs if e["kind"] in ("mic_utterance", "stop_intent", "keyword_stop")]
    record(
        "unwoken-media",
        still and bool(ignored) and not acted,
        f"still_music={still} unwoken={[e.get('text') for e in ignored]} acted={[e['kind'] for e in acted]}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    time.sleep(0.5)


def check_no_cmd_idle():
    wait_idle()
    settle()
    close_window()
    t0 = now_ms()
    say_it("cmd_pause_volc.wav")
    ignored = []
    for _ in range(60):
        time.sleep(0.2)
        evs = events_since(t0, {"mic_ignored_unwoken", "mic_utterance", "stop_intent", "keyword_stop"})
        ignored = [e for e in evs if e["kind"] == "mic_ignored_unwoken"]
        if ignored:
            break
    evs = events_since(t0, {"mic_ignored_unwoken", "mic_utterance", "stop_intent", "keyword_stop"})
    acted = [e for e in evs if e["kind"] in ("mic_utterance", "stop_intent", "keyword_stop")]
    record(
        "unwoken-idle",
        bool(ignored) and not acted,
        f"unwoken={[e.get('text') for e in ignored]} acted={[e['kind'] for e in acted]}",
    )


def check_voice_pause_resume_retreat():
    """The flow a person actually uses on a half-duplex link (user, 2026-11-12):
    "你好高达" first, then the command. The wake stops the story and keeps the
    place; 继续 reads on from the sentence boundary; a second wake + 退下
    clears the amber.

    Everything is polled, nothing is timed by a fixed sleep: the story is a
    real model turn whose generation/streaming pace varies."""
    wait_idle(close_window=True)
    settle()
    clear_device_session()
    t0 = now_ms()
    # A REAL turn through the device entrance (the console's /chat endpoint
    # only streams to its own SSE client — it never makes the speaker talk).
    say_it("wake_story_volc.wav")  # "你好高达，讲一个很长的童话故事。"
    if not wait_activity("speaking", 30):
        record("voice-pause-resume-retreat", False,
               f"premise failed: the story never spoke ({dev().get('activity')})")
        return
    # Let the program accumulate enough text that resuming is observable.
    have_text = False
    for _ in range(60):
        live = dev()["program"].get("live") or {}
        if live.get("chars", 0) >= 120:
            have_text = True
            break
        time.sleep(0.25)
    if not have_text:
        record("voice-pause-resume-retreat", False, "premise failed: the program text never grew")
        return

    # 1) wake during the story -> stop + bookmark
    say_it("wake_ni_gaoda_volc.wav")
    bookmark = None
    for _ in range(60):
        time.sleep(0.2)
        bookmark = dev()["program"].get("paused")
        if bookmark:
            break
    # 2) 继续 in the open window. A short pause: right after the stop the
    # answered tail and the 400 ms post-play blind window can still eat the
    # first syllable — a person also waits for the audio to be gone.
    time.sleep(1.2)
    resumed, tries = say_continue_and_wait(t0)
    # 3) wake again, then 退下
    say_it("wake_ni_gaoda_volc.wav")
    time.sleep(2.5)
    say_it("retreat_volc.wav")
    cleared = False
    for _ in range(80):
        time.sleep(0.2)
        d = dev()
        if d["program"]["paused"] is None and d["session"]["state"] == "idle":
            cleared = True
            break
    evs = events_since(t0, {"session_retreat", "program_resumed", "program_finished",
                            "program_bookmarked_after_interrupt", "keyword_stop"})
    record(
        "voice-pause-resume-retreat",
        bool(bookmark) and resumed and cleared,
        f"bookmark_after_wake={bool(bookmark)} cursor={bookmark.get('cursor') if bookmark else None} "
        f"resumed={resumed} ({tries} tries) amber_cleared={cleared} "
        f"events={[e['kind'] for e in evs]}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})


# --- R1h: weather as a REGISTERED service + the truncation label -------------------

WEATHER_DENIALS = ("没有天气", "没有这个功能", "做不到", "没有天气服务", "无法查询天气")


def check_weather_now():
    """RH-2/RH-3: a registered service is used like any other tool. The proof
    is the tool call plus the real fetch plus an answer that carries a number
    — not the model's own claim."""
    wait_idle(close_window=True)
    settle()
    sid = clear_device_session()
    t0 = now_ms()
    say_it("weather_now_volc.wav")
    wait_speech_done(timeout=45)
    time.sleep(1.0)
    trace, detail, answer = last_turn_since(t0)
    evs = events_since(t0, {"weather_fetch", "weather_fetch_failed", "weather_stale_served"})
    tools = tool_events(detail)
    called = [e for e in tools if e.get("kind") == "tool_start" and e.get("tool") == "weather.now"]
    results = [e for e in tools if e.get("kind") == "tool_result" and e.get("tool") == "weather.now"]
    # The trace stores the Rust Debug form ("Completed"); the tool contract
    # is case-insensitive here.
    ok_result = [e for e in results if str(e.get("status", "")).lower() == "completed"]
    fetched = [e for e in evs if e.get("kind") == "weather_fetch"]
    # A reading younger than the freshness window answers from cache, so a
    # missing fetch event is fine — the TOOL RESULT is what must carry real
    # data (and it says how old it is).
    result_payload = (ok_result[0].get("result") or {}) if ok_result else {}
    has_number = any(ch.isdigit() for ch in answer)
    denied = [d for d in WEATHER_DENIALS if d in answer]
    record(
        "weather-now",
        bool(called) and bool(ok_result) and has_number and not denied,
        f"session={sid} tool_called={bool(called)} tool_ok={bool(ok_result)} "
        f"fresh_fetch={[e.get('temperature_c') for e in fetched]} "
        f"tool_temp_c={result_payload.get('temperature_c')} age_ms={result_payload.get('age_ms')} "
        f"condition={result_payload.get('condition')!r} answer={answer[:90]!r} denied={denied}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    time.sleep(0.5)


def check_weather_unconfigured():
    """RH-2: switched off is a LOCAL state — the tool says so and the provider
    sees nothing (no weather_fetch / weather_fetch_failed event)."""
    wait_idle(close_window=True)
    settle()
    set_weather_enabled(False)
    clear_device_session()
    time.sleep(1.0)
    t0 = now_ms()
    say_it("weather_now_volc.wav")
    wait_speech_done(timeout=45)
    time.sleep(1.0)
    trace, detail, answer = last_turn_since(t0)
    tools = tool_events(detail)
    results = [e for e in tools if e.get("kind") == "tool_result"]
    codes = [((e.get("error") or {}).get("code")) for e in results]
    http_evs = events_since(t0, {"weather_fetch", "weather_fetch_failed", "weather_stale_served"})
    honest = any(c in ("WEATHER_DISABLED", "WEATHER_UNCONFIGURED") for c in codes)
    mentions = any(w in answer for w in ("配置", "关闭", "不可用", "没法", "暂时"))
    record(
        "weather-unconfigured",
        (honest or mentions) and not http_evs,
        f"tool_error_codes={codes} http_events={len(http_evs)} answer={answer[:80]!r}",
    )
    set_weather_enabled(True)
    time.sleep(1.0)
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    time.sleep(0.5)


def check_weather_interrupt():
    """RH-4: a weather answer is a program like any other. The wake word stops
    it and keeps the place; 继续 reads on. Nothing weather-specific happens."""
    wait_idle(close_window=True)
    settle()
    clear_device_session()
    t0 = now_ms()
    say_it("weather_forecast_volc.wav")
    if not wait_activity("speaking", 45):
        record("weather-interrupt", False, f"premise failed: the answer never spoke ({dev().get('activity')})")
        return
    have_text = False
    for _ in range(120):
        live = dev()["program"].get("live") or {}
        if live.get("chars", 0) >= 60 and dev().get("activity") == "speaking":
            have_text = True
            break
        time.sleep(0.25)
    if not have_text:
        record("weather-interrupt", False, "premise failed: the answer text never grew while speaking")
        return
    say_it("wake_ni_gaoda_volc.wav")
    bookmark = None
    for _ in range(80):
        time.sleep(0.2)
        bookmark = dev()["program"].get("paused")
        if bookmark:
            break
    time.sleep(1.2)
    resumed, tries = say_continue_and_wait(t0)
    evs = events_since(t0, {"keyword_stop", "wake_word_stop", "program_bookmarked_after_interrupt",
                            "program_resumed"})
    weather_tool = [e for e in evs if e["kind"].startswith("weather_")]
    record(
        "weather-interrupt",
        bool(bookmark) and resumed,
        f"bookmark={bool(bookmark)} cursor={(bookmark or {}).get('cursor')} complete={(bookmark or {}).get('complete')} "
        f"resumed={resumed} ({tries} tries) events={[e['kind'] for e in evs]}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})


def check_truncation():
    """RH-1: a program cut WHILE THE MODEL IS STILL WRITING cannot be finished
    from its bookmark. 继续 reads the prefix and then says the configured
    truncation notice — no regeneration (zero new traces)."""
    phrase = "后面还没写完，先读到这里。"
    # The phrase is household configuration: top-level, not per device.
    default_phrase = mode().get("truncation_notice") or ""
    wait_idle(close_window=True)
    settle()
    clear_device_session()
    put(f"/api/v0/mode/{DEVICE}", {"truncation_notice": phrase})
    time.sleep(0.5)
    t0 = now_ms()
    # A LONG story: generation must still be running when the wake word
    # lands, otherwise the bookmark is legitimately complete and there is
    # nothing truncated to label.
    say_it("wake_long_story_volc.wav")
    if not wait_activity("speaking", 45):
        record("truncation", False, f"premise failed: the story never spoke ({dev().get('activity')})")
        put(f"/api/v0/mode/{DEVICE}", {"truncation_notice": default_phrase})
        return
    # Interrupt EARLY: the story must still be generating when the wake word
    # lands (the stimulus itself takes ~1.2 s to play). 30 chars is enough
    # text for 继续 to have something to read.
    incomplete = False
    for _ in range(120):
        live = dev()["program"].get("live") or {}
        if live.get("chars", 0) >= 30 and live.get("complete") is False:
            incomplete = True
            break
        time.sleep(0.15)
    if not incomplete:
        live = dev()["program"].get("live") or {}
        record("truncation", False,
               f"premise failed: generation had already finished (chars={live.get('chars')} "
               f"complete={live.get('complete')}); ask for a longer story")
        post(f"/api/v0/devices/{DEVICE}/stop", {})
        put(f"/api/v0/mode/{DEVICE}", {"truncation_notice": default_phrase})
        return
    say_it("wake_ni_gaoda_volc.wav")
    bookmark = None
    for _ in range(80):
        time.sleep(0.2)
        bookmark = dev()["program"].get("paused")
        if bookmark:
            break
    resume_ids = trace_ids()
    time.sleep(1.2)
    say_continue_and_wait(t0, attempts=2)
    notice = None
    for _ in range(150):
        time.sleep(0.2)
        evs = events_since(t0, {"program_truncation_notice", "program_truncation_notice_skipped",
                                "program_finished", "program_resumed"})
        notice = next((e for e in evs if e["kind"] == "program_truncation_notice"), None)
        if notice:
            break
    evs = events_since(t0, {"program_truncation_notice", "program_truncation_notice_skipped",
                            "program_finished", "program_resumed", "program_interrupted"})
    finished = next((e for e in reversed(evs) if e["kind"] == "program_finished"), None)
    new_traces = new_traces_since(resume_ids)
    record(
        "truncation",
        bool(bookmark) and bookmark.get("complete") is False and bool(notice)
        and notice.get("phrase") == phrase and not new_traces,
        f"bookmark_complete={(bookmark or {}).get('complete')} cursor={(bookmark or {}).get('cursor')} "
        f"notice={(notice or {}).get('phrase')!r} source={(notice or {}).get('source')} "
        f"finished_complete={(finished or {}).get('complete')} "
        f"new_traces_after_resume={[(t.get('input_text') or '')[:20] for t in new_traces]} "
        f"events={[e['kind'] for e in evs]}",
    )
    post(f"/api/v0/devices/{DEVICE}/stop", {})
    put(f"/api/v0/mode/{DEVICE}", {"truncation_notice": default_phrase})
    time.sleep(0.5)


CHECKS = {
    "kws-idle": check_kws_idle,
    "kws-media": check_kws_media,
    "unwoken-story": check_no_cmd_story,
    "unwoken-media": check_no_cmd_media,
    "unwoken-idle": check_no_cmd_idle,
    "voice-pause-resume-retreat": check_voice_pause_resume_retreat,
    "weather-now": check_weather_now,
    "weather-unconfigured": check_weather_unconfigured,
    "weather-interrupt": check_weather_interrupt,
    "truncation": check_truncation,
}


def main():
    os.makedirs(OUT, exist_ok=True)
    names = [a for a in sys.argv[1:] if not a.startswith("-")] or list(CHECKS)
    unknown = [n for n in names if n not in CHECKS]
    if unknown:
        raise SystemExit(f"unknown checks: {unknown}; known: {list(CHECKS)}")
    print(f"# R1g matrix acceptance {time.strftime('%Y-%m-%d %H:%M:%S')} core={CORE}")
    for n in names:
        CHECKS[n]()
    # Leave the household the way we found it: an acceptance run speaks story
    # and weather requests, and a leftover request in the conversation makes
    # the NEXT turn ("笑。") continue it (device log 2026-11-12 19:09: a laugh
    # became a turn and the model told a story nobody asked for).
    try:
        clear_device_session()
        post(f"/api/v0/devices/{DEVICE}/stop", {})
    except Exception as e:  # noqa: BLE001 - cleanup must not mask results
        print(f"# cleanup failed: {e}")
    fails = [r for r in results if not r[1]]
    with open(os.path.join(OUT, "r1g-matrix.jsonl"), "a") as f:
        for name, ok, detail in results:
            f.write(json.dumps({"ts": now_ms(), "check": name, "pass": ok, "detail": detail}) + "\n")
    print(f"# {len(results) - len(fails)}/{len(results)} passed; log {OUT}/r1g-matrix.jsonl")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
