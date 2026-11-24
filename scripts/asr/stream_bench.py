#!/usr/bin/env python3
"""T2 phase 1: streaming-ASR bench (measurement only, no runtime wiring).

Matrix (user 2026-11-10, corrected for account entitlements — see
docs/asr-service-plan.md §9):
  - volc-stream : Volcano 大模型流式 ASR, wss streaming, the ONLY true
                  streaming endpoint we can use (resource
                  volc.seedasr.sauc.duration, granted)
  - sf-batch    : SiliconFlow SenseVoice batch (WAN baseline, in production)
  - omlx-batch  : local oMLX Qwen3-ASR-0.6B-4bit (LAN baseline)

No Volcano batch baseline: this account has no AUC/AUC-Turbo entitlement
(403 resource not granted). No SiliconFlow streaming: their ASR is
file-based HTTP only (official docs snapshot cited in the bench report).

Metrics, per category (wake / retreat / command / long, >= 30 items each):
  t_intent  speech end -> first DEFINITE transcript containing the keyword
            (streaming only; batch has no intent signal by construction)
  t_final   speech end -> final transcript
  accuracy  CER + normalized exact match; keyword exact-match rate for
            wake/retreat
Times are milliseconds relative to the detected end of speech (trailing
silence removed), p50/p95 reported.

Real-speech subset: the six ASR-anchored windows from the 2026-11-10
KWS session (/tmp/hm-noise-forensic/run5/wake-00..05.wav) plus the whole
120 s post-AEC track (post.pcm) as continuous speech. The hum-01..19
energy-threshold segments are deliberately NOT used (bad material, 17/19
unreadable by ASR — see lessons D-26).

Usage:
  scripts/asr/stream_bench.py corpus      # synthesize + cache the corpus
  scripts/asr/stream_bench.py bench       # run the three providers
  scripts/asr/stream_bench.py real        # real-speech subset only
  scripts/asr/stream_bench.py concurrency # 1..3 simultaneous Volcano streams
  scripts/asr/stream_bench.py vad         # VAD hangover experiment (post.pcm)
  scripts/asr/stream_bench.py all
"""
import asyncio
import base64
import json
import os
import re
import struct
import subprocess
import sys
import time
import unicodedata
import urllib.request
import wave

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
CACHE = os.path.join(ROOT, "data", "asr", "bench")
RUN5 = "/tmp/hm-noise-forensic/run5"
CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")


# --- credentials (never printed) -------------------------------------------------

def env_file(key):
    for path in (os.path.expanduser("~/.hermes/.env"), os.path.join(ROOT, "data", "env.local")):
        if not os.path.exists(path):
            continue
        for line in open(path, encoding="utf-8", errors="replace"):
            m = re.match(r"^([A-Z_]+)=(.*)$", line.rstrip("\n"))
            if m and m.group(1) == key:
                return m.group(2).strip().strip('"').strip("'")
    raise SystemExit(f"missing {key}")


def omlx_key():
    with open(os.path.expanduser("~/.omlx/settings.json"), encoding="utf-8") as f:
        return json.load(f)["auth"]["api_key"]


# --- corpus ----------------------------------------------------------------------

def c3(texts):
    """10 distinct x 3 repeats = 30 items (latency distribution + accuracy)."""
    out = []
    for i, t in enumerate(texts):
        for r in range(3):
            out.append({"text": t, "variant": f"{i}-{r}"})
    return out


WAKE_TEXTS = ["你好，高达", "喂，高达", "高达", "嘿，高达", "嗯，高达",
              "你好呀，高达", "早啊，高达", "高达，在吗", "那个，高达", "嗨，高达"]
RETREAT_TEXTS = ["退下", "退一下吧", "先退下", "休息吧", "没事了",
                 "不用了", "结束吧", "到这吧", "退吧", "休息了"]
COMMAND_TEXTS = [
    "把客厅的灯关掉", "打开卧室的灯", "现在几点了", "明天天气怎么样",
    "放一首周杰伦的歌", "声音小一点", "声音大一点", "讲个笑话",
    "讲个故事", "放点轻音乐", "暂停一下", "继续播放",
    "刚才那首歌叫什么", "明天早上七点叫我", "今天星期几", "帮我查一下快递",
    "窗帘拉上", "把音量调到一半", "播放下一条新闻", "我饿了",
    "附近有什么好吃的", "提醒我十分钟后喝水", "这首歌是谁唱的", "换一首歌",
    "把灯调暗一点", "客厅太亮了", "帮我定个闹钟", "现在外面下雨吗",
    "说说明天的安排", "我想听相声",
]
LONG_TEXTS = [
    "今天天气不错，我想出去走走，你帮我看看下午会不会下雨，要是下雨的话我就在家待着。",
    "明天早上我要早起去公司开会，你帮我定一个六点半的闹钟，顺便提醒我带电脑和门禁卡。",
    "讲个故事吧，从前有一只小兔子住在森林边上，它每天早上都会去河边喝水，有一天它遇到了一只小刺猬。",
    "我最近睡眠不太好，晚上总是醒，你有什么办法能帮我放松一下吗，比如放点白噪音或者轻音乐。",
    "帮我想想周末干什么，如果天气好的话可以去公园野餐，如果下雨的话就去看电影或者在家做饭。",
    "刚才那首歌我觉得一般，换一首节奏轻快一点的，最好是我没听过的，别老是那几首。",
    "家里的灯好像有点问题，客厅那盏有时候会闪，你帮我看看是不是灯泡松了还是应该换了。",
    "我明天要出差三天，你帮我把家里的事情安排一下，记得提醒我给花浇水，还有垃圾分类。",
    "这个周末我爸妈要来，我想准备几个菜，你帮我列一个购物清单，最好简单一点不要太复杂。",
    "最近工作压力有点大，晚上回来只想安静一会儿，你能不能陪我聊聊天，说说轻松的话题。",
]

CORPUS = {
    "wake": c3(WAKE_TEXTS),
    "retreat": c3(RETREAT_TEXTS),
    "command": [{"text": t, "variant": str(i)} for i, t in enumerate(COMMAND_TEXTS)],
    "long": c3(LONG_TEXTS),
}

KEYWORDS = {
    "wake": ["高达"],
    "retreat": ["退下", "退一下", "退吧", "休息吧", "休息了", "没事了", "不用了", "结束吧", "到这吧"],
}


def synth_wav(text, path):
    """Volcano unidirectional TTS, same params as the device voice."""
    body = json.dumps({
        "user": {"uid": "home-mate"},
        "req_params": {
            "text": text,
            "speaker": "zh_female_xiaohe_uranus_bigtts",
            "audio_params": {"format": "pcm", "sample_rate": 16000},
        },
    }).encode()
    req = urllib.request.Request(
        "https://openspeech.bytedance.com/api/v3/tts/unidirectional",
        data=body,
        headers={"X-Api-Key": env_file("VOLC_API_KEY"), "X-Api-Resource-Id": "seed-tts-2.0",
                 "Content-Type": "application/json"},
    )
    pcm = bytearray()
    with urllib.request.urlopen(req, timeout=90) as r:
        for line in r:
            line = line.strip()
            if not line:
                continue
            try:
                f = json.loads(line)
            except json.JSONDecodeError:
                continue
            if f.get("code", 0) not in (0, 20000000):
                raise RuntimeError(f"tts frame error: {f}")
            d = f.get("data")
            if isinstance(d, str) and d:
                pcm.extend(base64.b64decode(d))
    if len(pcm) < 2000:
        raise RuntimeError(f"tts produced {len(pcm)} bytes")
    hdr = b"RIFF" + struct.pack("<I", 36 + len(pcm)) + b"WAVEfmt " + struct.pack(
        "<IHHIIHH", 16, 1, 1, 16000, 32000, 2, 16) + b"data" + struct.pack("<I", len(pcm))
    with open(path, "wb") as f:
        f.write(hdr + bytes(pcm))
    return len(pcm) / 2 / 16000


def cmd_corpus():
    os.makedirs(CACHE, exist_ok=True)
    manifest_path = os.path.join(CACHE, "manifest.json")
    manifest = json.load(open(manifest_path)) if os.path.exists(manifest_path) else {}
    for cat, items in CORPUS.items():
        os.makedirs(os.path.join(CACHE, cat), exist_ok=True)
        for i, item in enumerate(items):
            key = f"{cat}/{i:03d}"
            path = os.path.join(CACHE, f"{key}.wav")
            if key in manifest and os.path.exists(path) and os.path.getsize(path) > 3000:
                continue
            dur = synth_wav(item["text"], path)
            manifest[key] = {"text": item["text"], "duration_s": round(dur, 2)}
            print(f"synth {key} {dur:.1f}s {item['text'][:24]}")
            json.dump(manifest, open(manifest_path, "w"), ensure_ascii=False, indent=1)
    print(f"corpus ready: {len(manifest)} items in {CACHE}")


# --- text metrics -----------------------------------------------------------------

DIGIT_MAP = str.maketrans("0123456789", "零一二三四五六七八九")


def norm(s):
    s = unicodedata.normalize("NFKC", s or "")
    s = re.sub(r"[\U0001F300-\U0001FAFF\u2600-\u27BF]", "", s)
    s = re.sub(r"[，。？！、,.!?~：:；;‘’“”\"'（）()《》\s]", "", s)
    return s.translate(DIGIT_MAP)


def cer(gt, hyp):
    a, b = norm(gt), norm(hyp)
    if not a:
        return 0.0 if not b else 1.0
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1] / len(a)


def keyword_hit(text, keywords):
    n = norm(text)
    return any(k in n for k in keywords)


def pct(vals, p):
    if not vals:
        return None
    vs = sorted(vals)
    k = max(0, min(len(vs) - 1, int(round((p / 100.0) * (len(vs) - 1)))))
    return vs[k]


# --- audio helpers ----------------------------------------------------------------

def read_pcm_wav(path):
    with wave.open(path) as w:
        assert w.getframerate() == 16000 and w.getnchannels() == 1, (path, w.getframerate(), w.getnchannels())
        return w.readframes(w.getnframes())


def speech_end_ms(pcm_bytes, thresh=300, margin_ms=50):
    """End of speech inside the file: last sample above `thresh`, minus a
    small margin (trailing digital silence must not dilute the latency)."""
    n = len(pcm_bytes) // 2
    samples = struct.unpack(f"<{n}h", pcm_bytes[: n * 2])
    last = 0
    for i in range(n - 1, -1, -1):
        if abs(samples[i]) >= thresh:
            last = i
            break
    return max(0.0, (last * 1000.0 / 16000.0) - margin_ms)


# --- providers --------------------------------------------------------------------

HDR_FULL = bytes([0x11, 0x10, 0x11, 0x00])
HDR_AUDIO = bytes([0x11, 0x20, 0x00, 0x00])
VOLC_WS = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel"
VOLC_RESOURCE = "volc.seedasr.sauc.duration"


def _pkt(hdr, payload):
    return hdr + struct.pack(">I", len(payload)) + payload


PACE = float(os.environ.get("HM_BENCH_PACE", "1.0"))
# Pre-roll experiment (user 2026-11-10): pad N ms of digital silence BEFORE
# the speech so the streaming decoder has start-of-utterance context. If the
# swallowed leading syllables come back, the CER gap was a wiring artifact.
PREROLL_MS = int(os.environ.get("HM_BENCH_PREROLL_MS", "0"))
# Padded pre-roll can also be sent as a leading BURST instead of paced: the
# context arrives before the speech without shifting the whole stream later
# (paced padding costs ~200ms of final latency - measured 2026-11-10).
PREROLL_BURST = os.environ.get("HM_BENCH_PREROLL_BURST", "0") == "1"
CATS = [c for c in os.environ.get("HM_BENCH_CATS", "").split(",") if c]


async def volc_stream(pcm, keywords, chunk_ms=200, idle_timeout=6.0):
    """One streaming recognition. Returns dict with final text, per-definite
    events and keyword-intent latency (ms relative to speech end)."""
    import gzip
    import uuid

    import websockets

    pad_bytes = 0
    if PREROLL_MS > 0:
        pad_bytes = 2 * (16000 * PREROLL_MS // 1000)
        pcm = b"\x00\x00" * (pad_bytes // 2) + pcm
    end_ms = speech_end_ms(pcm)
    t_start = time.perf_counter()

    def rel():
        return (t_start - t_start) * 0 + (time.perf_counter() - t_start) * 1000.0 - end_ms

    headers = {
        "X-Api-Key": env_file("VOLC_API_KEY"),
        "X-Api-Resource-Id": VOLC_RESOURCE,
        "X-Api-Connect-Id": str(uuid.uuid4()),
    }
    ws = await websockets.connect(VOLC_WS, additional_headers=headers, max_size=None, proxy=None)
    events = []
    state = {"final": None, "final_ms": None, "intent_ms": None}

    async def reader():
        while True:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=idle_timeout)
            except Exception:
                return
            mt = msg[1] >> 4
            flags = msg[1] & 0x0F
            if mt == 0xF:
                body = msg[4:]
                try:
                    body = gzip.decompress(body)
                except Exception:
                    pass
                state["error"] = body.decode("utf-8", "replace")[:300]
                return
            off = 4
            if flags & 0x01:
                off += 4
            size = struct.unpack(">I", msg[off:off + 4])[0]
            off += 4
            body = msg[off:off + size]
            try:
                body = gzip.decompress(body)
            except Exception:
                pass
            try:
                data = json.loads(body)
            except Exception:
                continue
            result = data.get("result") or {}
            text = result.get("text") or ""
            utts = result.get("utterances") or []
            t = rel()
            events.append({"t_ms": round(t), "text": text,
                           "definite": [u.get("definite") for u in utts]})
            definite_text = "".join(u.get("text", "") for u in utts if u.get("definite"))
            if definite_text and state["intent_ms"] is None and keyword_hit(definite_text, keywords):
                state["intent_ms"] = t
            if utts and all(u.get("definite") for u in utts):
                state["final"] = text
                state["final_ms"] = t
                return

    rt = asyncio.create_task(reader())
    req = {
        "user": {"uid": "home-mate"},
        "audio": {"format": "pcm", "codec": "raw", "rate": 16000, "bits": 16, "channel": 1, "language": "zh"},
        "request": {"model_name": "bigmodel", "enable_itn": True, "enable_punc": True,
                    "result_type": "single", "show_utterances": True, "end_window_size": 500},
    }
    await ws.send(_pkt(HDR_FULL, gzip.compress(json.dumps(req).encode())))
    step = int(16000 * 2 * chunk_ms / 1000)
    start_at = 0
    if pad_bytes and PREROLL_BURST:
        # Leading context burst: no pacing for the padding, then real time.
        await ws.send(_pkt(HDR_AUDIO, pcm[:pad_bytes]))
        start_at = pad_bytes
    for i in range(start_at, len(pcm), step):
        await ws.send(_pkt(HDR_AUDIO, pcm[i:i + step]))
        await asyncio.sleep(chunk_ms / 1000 * PACE)
    await ws.send(_pkt(bytes([0x11, 0x22, 0x00, 0x00]), b""))
    try:
        await asyncio.wait_for(rt, timeout=idle_timeout)
    except asyncio.TimeoutError:
        rt.cancel()
    try:
        await ws.close()
    except Exception:
        pass
    if state["final"] is None and events:
        last = [e for e in events if e["text"]]
        state["final"] = last[-1]["text"] if last else ""
        state["final_ms"] = last[-1]["t_ms"] if last else None
    return {"provider": "volc-stream", "final": state["final"] or "", "final_ms": state["final_ms"],
            "intent_ms": state["intent_ms"], "events": events, "error": state.get("error")}


def batch_transcribe(pcm, base_url, key, model):
    """OpenAI-compatible batch transcription; t_final = full HTTP RTT."""
    boundary = "----hmbench" + str(int(time.time() * 1000))
    wav = b"RIFF" + struct.pack("<I", 36 + len(pcm)) + b"WAVEfmt " + struct.pack(
        "<IHHIIHH", 16, 1, 1, 16000, 32000, 2, 16) + b"data" + struct.pack("<I", len(pcm)) + pcm
    parts = []
    parts.append(f"--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n".encode())
    parts.append(f"--{boundary}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\nzh\r\n".encode())
    parts.append((f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"u.wav\"\r\n"
                  f"Content-Type: audio/wav\r\n\r\n").encode() + wav + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    body = b"".join(parts)
    req = urllib.request.Request(
        base_url.rstrip("/") + "/audio/transcriptions", data=body,
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}",
                 "Authorization": f"Bearer {key}"},
    )
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.load(r)
    ms = (time.perf_counter() - t0) * 1000.0
    return {"provider": "batch", "final": (data.get("text") or "").strip(), "final_ms": ms, "intent_ms": None}


# --- bench ------------------------------------------------------------------------

def summarize(rows):
    """rows: list of dicts with provider/category/gt/final/final_ms/intent_ms."""
    out = {}
    by = {}
    for r in rows:
        by.setdefault((r["provider"], r["category"]), []).append(r)
    for (prov, cat), rs in sorted(by.items()):
        finals = [r["final_ms"] for r in rs if r.get("final_ms") is not None]
        intents = [r["intent_ms"] for r in rs if r.get("intent_ms") is not None]
        cers = [cer(r["gt"], r["final"]) for r in rs]
        exact = sum(1 for r in rs if norm(r["gt"]) == norm(r["final"])) / len(rs)
        kw = None
        if cat in KEYWORDS:
            kw = sum(1 for r in rs if keyword_hit(r["final"], KEYWORDS[cat])) / len(rs)
        out[f"{prov}/{cat}"] = {
            "n": len(rs),
            "t_final_p50": round(pct(finals, 50), 1) if finals else None,
            "t_final_p95": round(pct(finals, 95), 1) if finals else None,
            "t_intent_p50": round(pct(intents, 50), 1) if intents else None,
            "t_intent_p95": round(pct(intents, 95), 1) if intents else None,
            "cer_mean": round(sum(cers) / len(cers), 4),
            "exact_match": round(exact, 3),
            "keyword_rate": round(kw, 3) if kw is not None else None,
        }
    return out


async def bench_async(real_only=False):
    sf_key = env_file("SILICONFLOW_API_KEY")
    om_key = omlx_key()
    rows, samples = [], []
    manifest = json.load(open(os.path.join(CACHE, "manifest.json"))) if os.path.exists(
        os.path.join(CACHE, "manifest.json")) else {}

    jobs = []
    if not real_only:
        for cat in CORPUS:
            if CATS and cat not in CATS:
                continue
            for i in range(len(CORPUS[cat])):
                key = f"{cat}/{i:03d}"
                wav = os.path.join(CACHE, f"{key}.wav")
                if not os.path.exists(wav):
                    continue
                jobs.append((cat, key, manifest.get(key, {}).get("text", CORPUS[cat][i]["text"]), wav))

    for cat, key, gt, wav in jobs:
        pcm = read_pcm_wav(wav)
        vr = await volc_stream(pcm, KEYWORDS.get(cat, []))
        rows.append({"provider": "volc-stream", "category": cat, "gt": gt, **{
            k: vr[k] for k in ("final", "final_ms", "intent_ms")}})
        samples.append({"corpus": key, "gt": gt, **{k: vr[k] for k in ("final", "final_ms", "intent_ms")},
                        "events": vr["events"][-3:]})
        sb = batch_transcribe(pcm, "https://api.siliconflow.cn/v1", sf_key, "FunAudioLLM/SenseVoiceSmall")
        rows.append({"provider": "sf-batch", "category": cat, "gt": gt,
                     "final": sb["final"], "final_ms": sb["final_ms"], "intent_ms": None})
        ob = batch_transcribe(pcm, "http://192.168.1.27:9981/v1", om_key, "Qwen3-ASR-0.6B-4bit")
        rows.append({"provider": "omlx-batch", "category": cat, "gt": gt,
                     "final": ob["final"], "final_ms": ob["final_ms"], "intent_ms": None})
        print(f"{key} volc:{vr['intent_ms']}/{vr['final_ms']} sf:{sb['final_ms']:.0f} "
              f"omlx:{ob['final_ms']:.0f} | {vr['final']!r}")

    # Real speech (KWS-session windows + full track).
    real_items = []
    for i in range(6):
        p = os.path.join(RUN5, f"wake-{i:02d}.wav")
        if os.path.exists(p):
            gt = "你好，高达" if i < 5 else "你好呀，高达"
            real_items.append((f"real/wake-{i:02d}", gt, p, "wake"))
    post = os.path.join(RUN5, "post.pcm")
    if os.path.exists(post):
        real_items.append(("real/post-track", None, post, "real"))

    for key, gt, path, cat in real_items:
        if path.endswith(".pcm"):
            pcm = open(path, "rb").read()
            gt = None
        else:
            pcm = read_pcm_wav(path)
        vr = await volc_stream(pcm, KEYWORDS.get(cat, []))
        samples.append({"corpus": key, "gt": gt, **{k: vr[k] for k in ("final", "final_ms", "intent_ms")},
                        "events": vr["events"][-3:]})
        if gt:
            rows.append({"provider": "volc-stream", "category": "real", "gt": gt, **{
                k: vr[k] for k in ("final", "final_ms", "intent_ms")}})
        print(f"{key} volc intent={vr['intent_ms']} final={vr['final_ms']} text={vr['final']!r}")
        if gt:
            sb = batch_transcribe(pcm, "https://api.siliconflow.cn/v1", sf_key, "FunAudioLLM/SenseVoiceSmall")
            rows.append({"provider": "sf-batch", "category": "real", "gt": gt,
                         "final": sb["final"], "final_ms": sb["final_ms"], "intent_ms": None})
            ob = batch_transcribe(pcm, "http://192.168.1.27:9981/v1", om_key, "Qwen3-ASR-0.6B-4bit")
            rows.append({"provider": "omlx-batch", "category": "real", "gt": gt,
                         "final": ob["final"], "final_ms": ob["final_ms"], "intent_ms": None})
            print(f"  batch sf={sb['final']!r} omlx={ob['final']!r}")

    summary = summarize(rows)
    out = {"generated_at": time.strftime("%Y-%m-%dT%H:%M:%S"), "summary": summary,
           "rows": rows, "samples": samples}
    path = os.path.join(CACHE, os.environ.get("HM_BENCH_OUT", "results.json"))
    json.dump(out, open(path, "w"), ensure_ascii=False, indent=1)
    print(json.dumps(summary, ensure_ascii=False, indent=1))
    print(f"results -> {path}")
    return out


# --- concurrency ------------------------------------------------------------------

async def concurrency_probe():
    import uuid

    import websockets

    key = env_file("VOLC_API_KEY")
    pcm = open(os.path.join(RUN5, "post.pcm"), "rb").read()[:16000 * 2 * 8]  # 8 s

    async def one(i):
        headers = {"X-Api-Key": key, "X-Api-Resource-Id": VOLC_RESOURCE, "X-Api-Connect-Id": str(uuid.uuid4())}
        try:
            ws = await websockets.connect(VOLC_WS, additional_headers=headers, max_size=None, proxy=None)
        except Exception as e:
            return i, "connect-fail", str(e)[:200]
        try:
            import gzip
            req = {"user": {"uid": "home-mate"},
                   "audio": {"format": "pcm", "codec": "raw", "rate": 16000, "bits": 16, "channel": 1},
                   "request": {"model_name": "bigmodel", "show_utterances": True}}
            await ws.send(_pkt(HDR_FULL, gzip.compress(json.dumps(req).encode())))
            step = 6400
            for off in range(0, len(pcm), step):
                await ws.send(_pkt(HDR_AUDIO, pcm[off:off + step]))
                await asyncio.sleep(0.18)
            await ws.send(_pkt(bytes([0x11, 0x22, 0x00, 0x00]), b""))
            got = 0
            t0 = time.perf_counter()
            while time.perf_counter() - t0 < 6:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=6)
                except websockets.exceptions.ConnectionClosedOK:
                    # The server closes with 1000 after the final result —
                    # that IS a successful session (caught live 2026-11-10:
                    # this branch was counted as a failure on all 3 streams).
                    return i, "ok", f"{got} frames, clean close"
                if msg[1] >> 4 == 0xF:
                    body = msg[4:]
                    try:
                        body = gzip.decompress(body)
                    except Exception:
                        pass
                    return i, "error-frame", body.decode("utf-8", "replace")[:200]
                got += 1
            return i, "ok", f"{got} frames"
        except Exception as e:
            return i, "fail", f"{type(e).__name__}: {e}"
        finally:
            try:
                await ws.close()
            except Exception:
                pass

    for n in (1, 2, 3):
        res = await asyncio.gather(*[one(i) for i in range(n)])
        print(f"concurrency={n}: {res}")
        await asyncio.sleep(2)


# --- VAD hangover experiment ------------------------------------------------------

def energy_db(samples):
    if not samples:
        return -120.0
    acc = sum(float(s) * s for s in samples) / len(samples)
    return 10.0 * __import__("math").log10(acc / (32768.0 ** 2) + 1e-12)


def vad_segments(pcm, hangover_ms, start_db=-47.0, end_db=-52.0,
                 start_blocks=3, preroll_blocks=3, block_ms=80, min_blocks=3, max_blocks=150):
    """Python mirror of crates/hm-core/src/mic.rs MicProcessor (the live
    parameters are the frozen VAD -47/-52). Segments are (start_idx, end_idx)
    in blocks — enough to compare hangovers on the same material."""
    n = len(pcm) // 2
    samples = struct.unpack(f"<{n}h", pcm[: n * 2])
    block = int(16000 * block_ms / 1000)
    blocks = [samples[i:i + block] for i in range(0, n - block + 1, block)]
    end_blocks = max(1, int(round(hangover_ms / block_ms)))
    segs, in_speech, above, below, preroll, cur = [], False, 0, 0, [], []
    for idx, b in enumerate(blocks):
        db = energy_db(b)
        loud, quiet = db >= start_db, db < end_db
        if not in_speech:
            preroll.append(idx)
            preroll = preroll[-preroll_blocks:]
            above = above + 1 if loud else 0
            if above >= start_blocks:
                in_speech, below, cur = True, 0, list(preroll)
                above = 0
            continue
        cur.append(idx)
        below = below + 1 if quiet else 0
        if below >= end_blocks or len(cur) >= max_blocks:
            if len(cur) >= min_blocks:
                segs.append((cur[0], cur[-1]))
            in_speech, above, below, preroll, cur = False, 0, 0, [], []
    return segs



async def volc_utterance_anchors(pcm):
    """Tight utterance anchors from the streaming ASR's own timestamps.

    The VAD experiment must not define its own truth: anchors come from a
    different observer (the streaming recognizer's utterance start/end), and
    a VAD "cut" is a segment boundary INSIDE such an interval. Paced at 1.0
    so the service sees real-time audio (the accepted send cadence).
    """
    import gzip
    import uuid

    import websockets

    headers = {
        "X-Api-Key": env_file("VOLC_API_KEY"),
        "X-Api-Resource-Id": VOLC_RESOURCE,
        "X-Api-Connect-Id": str(uuid.uuid4()),
    }
    ws = await websockets.connect(VOLC_WS, additional_headers=headers, max_size=None, proxy=None)
    seen = {}

    async def reader():
        while True:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=8.0)
            except Exception:
                return
            mt = msg[1] >> 4
            flags = msg[1] & 0x0F
            off = 4
            if flags & 0x01:
                off += 4
            size = struct.unpack(">I", msg[off:off + 4])[0]
            off += 4
            body = msg[off:off + size]
            try:
                body = gzip.decompress(body)
            except Exception:
                pass
            if mt == 0xF:
                return
            try:
                data = json.loads(body)
            except Exception:
                continue
            for u in (data.get("result") or {}).get("utterances") or []:
                if not u.get("definite"):
                    continue
                st, en = u.get("start_time"), u.get("end_time")
                text = (u.get("text") or "").strip()
                if st is None or en is None or not text:
                    continue
                key = (st, en)
                seen[key] = text

    rt = asyncio.create_task(reader())
    # Volcano request framing, same as volc_stream (audio-only, gzip payloads).
    chunk = 2 * (16000 * 200 // 1000)
    req = {
        "user": {"uid": "home-mate"},
        "audio": {"format": "pcm", "codec": "raw", "rate": 16000, "bits": 16, "channel": 1, "language": "zh"},
        "request": {"model_name": "bigmodel", "enable_itn": True, "enable_punc": True,
                    "result_type": "single", "show_utterances": True, "end_window_size": 500},
    }
    await ws.send(_pkt(HDR_FULL, gzip.compress(json.dumps(req).encode())))
    step = int(16000 * 2 * 200 / 1000)
    for i in range(0, len(pcm), step):
        await ws.send(_pkt(HDR_AUDIO, pcm[i:i + step]))
        await asyncio.sleep(0.2 * PACE)
    await ws.send(_pkt(bytes([0x11, 0x22, 0x00, 0x00]), b""))
    try:
        await asyncio.wait_for(rt, timeout=8.0)
    except asyncio.TimeoutError:
        rt.cancel()
    try:
        await ws.close()
    except Exception:
        pass
    anchors = sorted([list(k) for k in seen.keys()])
    merged = []
    for a, b in anchors:
        if merged and a <= merged[-1][1] + 300:
            merged[-1][1] = max(merged[-1][1], b)
        else:
            merged.append([a, b])
    return [[round(a / 1000, 2), round(b / 1000, 2)] for a, b in merged]


def cmd_vad_anchors():
    pcm_path = os.environ.get("HM_VAD_PCM", os.path.join(RUN5, "post.pcm"))
    out_path = os.environ.get("HM_VAD_ANCHORS", "/tmp/hm-t4/vad-anchors.json")
    pcm = open(pcm_path, "rb").read()
    anchors = asyncio.run(volc_utterance_anchors(pcm))
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    json.dump(anchors, open(out_path, "w"), indent=1)
    print(f"{pcm_path}: {len(pcm)/2/16000:.0f}s -> {len(anchors)} utterance anchors -> {out_path}")
    for a in anchors[:8]:
        print(f"  {a[0]:7.2f}-{a[1]:7.2f}s")


def cmd_vad():
    """VAD hangover replay: does cutting the hangover split an utterance?

    Default (no args) keeps the original 120 s post-AEC material and its six
    ASR-anchored phrase windows. The generalized form takes ANY length of
    material plus anchors, because the landing gate for a hangover change is
    ">= 10 min of real speech including hesitations and word-final pauses,
    false-cut rate < 2%" (user ruling 2026-11-10) — the 120 s clip cannot
    carry that verdict.

      HM_VAD_PCM=/path/track.pcm HM_VAD_ANCHORS=/path/anchors.json \
        scripts/asr/stream_bench.py vad

    anchors.json = [[start_s, end_s], ...] — utterance windows obtained from
    ASR (sliding-window transcription, merged when consecutive windows both
    carry text). A phrase counts as CUT when two or more VAD segments overlap
    its window; the false-cut rate is cut/anchors.
    """
    pcm_path = os.environ.get("HM_VAD_PCM", os.path.join(RUN5, "post.pcm"))
    anchors_path = os.environ.get("HM_VAD_ANCHORS")
    hangovers = tuple(float(x) for x in os.environ.get("HM_VAD_HANGOVERS", "350,400,500,600").split(","))
    pcm = open(pcm_path, "rb").read()
    dur = len(pcm) / 2 / 16000
    if anchors_path:
        anchors = [tuple(a) for a in json.load(open(anchors_path))]
    else:
        # The original 120 s track: ASR-anchored windows from the KWS session
        # (full plan §9.1: every window was verified to contain the phrase).
        anchors = [(54.6, 63.2), (63.6, 72.4), (75.6, 84.4), (84.6, 93.2), (93.6, 102.2), (105.6, 114.2)]
    print(f"{pcm_path}: {dur:.0f}s; mirror VAD -47/-52; block 80ms; anchors={len(anchors)}")
    out = {"track": pcm_path, "duration_s": dur, "anchors": anchors, "by_hangover": {}}
    for hang in hangovers:
        segs = vad_segments(pcm, hang)
        # A CUT is a VAD segment boundary strictly INSIDE an utterance
        # interval: the recognizer's utterance is then delivered as two
        # segments (half-sentence turns). Overlap counting is wrong — a
        # padded window trivially spans the ACK and the phrase, which is how
        # the old "0/6" figure became meaningless (see docs).
        cuts, lead_clips, tail_clips = 0, 0, 0
        cut_dbg = []
        for (a, b) in anchors:
            if b - a < 0.3:
                continue
            covered = [(max(a, s * 0.08), min(b, e * 0.08)) for (s, e) in segs
                       if min(b, e * 0.08) > max(a, s * 0.08)]
            # A CUT is an utterance delivered as TWO segments: at least two
            # segments each own a real slice of it (>=150 ms). A single
            # segment plus an uncovered sliver is a clip, not a cut.
            parts = [c for c in covered if c[1] - c[0] >= 0.15]
            if len(parts) >= 2:
                cuts += 1
                cut_dbg.append((round(a, 2), round(b, 2)))
            elif parts:
                if parts[0][0] - a > 0.25:
                    lead_clips += 1
                if b - parts[-1][1] > 0.25:
                    tail_clips += 1
        split = cuts
        frag = []
        for i, (s, e) in enumerate(segs):
            d = (e - s + 1) * 0.08
            if d < 1.2:
                for (s2, e2) in segs:
                    if (s2, e2) != (s, e) and abs(s2 * 0.08 - e * 0.08) < 0.5:
                        frag.append((round(s * 0.08, 2), round(d, 2)))
                        break
        rate = 100.0 * split / max(1, len(anchors))
        # A hangover is only admissible at the user's gate when the false-cut
        # rate is below 2% AND the material is long enough to mean it.
        gate_len = dur >= 600
        verdict = ("PASS" if (rate < 2.0 and gate_len) else
                   ("rate-ok but material <10min: evidence not admissible" if rate < 2.0 else "FAIL"))
        out["by_hangover"][int(hang)] = {
            "segments": len(segs), "anchors_cut": split, "false_cut_rate_pct": round(rate, 2),
            "fragments": len(frag), "verdict": verdict, "frag_dbg": frag[:6],
            "lead_clips": lead_clips, "tail_clips": tail_clips, "cut_dbg": cut_dbg,
        }
        print(f"hangover {int(hang)}ms: segments={len(segs)} cut={split}/{len(anchors)} "
              f"false_cut={rate:.1f}% lead_clip={lead_clips} tail_clip={tail_clips} "
              f"fragments={len(frag)} -> {verdict}")
    json.dump(out, open(os.path.join(CACHE, "vad_hangover.json"), "w"), indent=1)



def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "all"
    if cmd in ("corpus", "all"):
        cmd_corpus()
    if cmd in ("bench", "all"):
        asyncio.run(bench_async(real_only=False))
    if cmd == "real":
        asyncio.run(bench_async(real_only=True))
    if cmd in ("concurrency", "all"):
        asyncio.run(concurrency_probe())
    if cmd in ("vad", "all"):
        cmd_vad()
    if cmd == "vad_anchors":
        cmd_vad_anchors()


if __name__ == "__main__":
    main()
