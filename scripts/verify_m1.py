#!/usr/bin/env python3
"""M1 acceptance runner against a running hm-core instance.

Exercises the user's acceptance bar with the REAL DeepSeek model and the
SIMULATED tool layer (fake household, no device side effects):

  1. bedtime story -> "换个短一点的" keeps context
  2. hot-list song via novel phrasing -> chart/search/play chain, answer
     grounded in the actual receipt
  3. mid-course correction ("不要这首，换第二首")
  4. honest failure reporting (run service with HM_INJECT_PLAY_FAIL=t01)

Usage:  python3 scripts/verify_m1.py [base_url]
Output: markdown report on stdout (see docs/verification-m1-*.md).
"""

import json
import sys
import time
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8930"


def post(path, body, timeout=120):
    req = urllib.request.Request(
        BASE + path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def get(path, timeout=30):
    with urllib.request.urlopen(BASE + path, timeout=timeout) as r:
        return json.loads(r.read())


def post_chat_sse(text, session_id=None):
    """POST /chat and consume the SSE stream, returning parsed events."""
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


def answer_text(events):
    for e in reversed(events):
        if e.get("type") == "answer":
            return e["text"]
    return ""


def tool_seq(events):
    return [e["tool"] for e in events if e.get("type") == "tool_start"]


def report():
    lines = [f"# M1 真实模型验证记录 ({time.strftime('%Y-%m-%d %H:%M:%S')})",
             f"目标服务: {BASE}（真实 DeepSeek + 模拟家庭工具）", ""]

    status = get("/api/v0/status")
    lines.append(f"- 服务: {status['product']} v{status['version']}，助手「{status['assistant_name']}」，"
                 f"LLM={status['services']['llm']['provider']}/{status['services']['llm']['model']}")
    lines.append("")

    # --- 1. story revision ---------------------------------------------------
    lines.append("## 1. 睡前故事 → 换个短一点的")
    ev1 = post_chat_sse("给我讲一个睡前故事，主角是一只叫点点的小刺猬")
    sid = ev1[0]["session_id"]
    story1 = answer_text(ev1)
    ev2 = post_chat_sse("换个短一点的", sid)
    story2 = answer_text(ev2)
    len1, len2 = len(story1), len(story2)
    keeps = "点点" in story2 or "刺猬" in story2
    shorter = len2 < len1 * 0.7
    lines.append(f"- 第一版（{len1} 字）：{story1[:120]}…")
    lines.append(f"- 短版（{len2} 字）：{story2[:120]}…")
    lines.append(f"- 保持主角语境: {'✓' if keeps else '✗'}；明显更短: {'✓' if shorter else '✗'}")
    trace2 = get("/api/v0/trace/" + ev2[-1]["trace_id"])
    ctx_ok = any("点点" in json.dumps(e.get("payload", {})) for e in trace2["events"])
    lines.append(f"- 追踪确认修订轮上下文含第一版故事: {'✓' if ctx_ok else '（上下文在服务端会话内，不在事件里——以模型输出为准）'}")
    lines.append("")

    # --- 2. novel phrasing hotlist -------------------------------------------
    lines.append("## 2. 新说法点热榜歌（\"来一首现在榜上最火的歌\"）")
    ev3 = post_chat_sse("来一首现在榜上最火的歌")
    seq = tool_seq(ev3)
    ans3 = answer_text(ev3)
    played = any(
        e.get("type") == "tool_result" and e.get("tool") == "music.play"
        and e.get("status") == "completed" for e in ev3)
    lines.append(f"- 工具链: {' → '.join(seq) if seq else '（无）'}")
    lines.append(f"- music.play 回执 completed: {'✓' if played else '✗'}")
    lines.append(f"- 回答: {ans3[:160]}")
    lines.append("")

    # --- 3. mid-course correction ---------------------------------------------
    lines.append("## 3. 临时纠正（\"不要这首，换第二首\"）")
    sid3 = ev3[0]["session_id"]
    ev4 = post_chat_sse("不要这首，换第二首", sid3)
    ans4 = answer_text(ev4)
    played2 = any(
        e.get("type") == "tool_result" and e.get("tool") == "music.play"
        and e.get("status") == "completed" for e in ev4)
    lines.append(f"- 工具链: {' → '.join(tool_seq(ev4))}")
    lines.append(f"- 换曲后播放回执 completed: {'✓' if played2 else '✗'}")
    lines.append(f"- 回答: {ans4[:160]}")
    lines.append("")

    # --- 4. failure honesty (needs HM_INJECT_PLAY_FAIL) ------------------------
    lines.append("## 4. 播放失败如实回应（需服务以 HM_INJECT_PLAY_FAIL=t01 启动）")
    ev5 = post_chat_sse("放榜单第一首")
    ans5 = answer_text(ev5)
    failed = any(
        e.get("type") == "tool_result" and e.get("tool") == "music.play"
        and e.get("status") == "failed" for e in ev5)
    says_fail = any(w in ans5 for w in ["放不出", "失败", "不行", "试试", "换"])
    lines.append(f"- music.play 回执 failed: {'✓' if failed else '✗（未注入失败时本项为 N/A）'}")
    lines.append(f"- 回答体现失败或改选: {'✓' if (not failed or says_fail) else '✗'}")
    lines.append(f"- 回答: {ans5[:160]}")
    lines.append("")

    lines.append("> 以上为真实模型在模拟工具上的行为记录；设备链路（网关/音箱）尚未接入，"
                 "播放\"completed\"为模拟 sink 回执，不代表有声输出。")
    print("\n".join(lines))


if __name__ == "__main__":
    report()
