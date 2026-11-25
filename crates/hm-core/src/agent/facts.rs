//! R2-BM5: the answer contract for action facts.
//!
//! R1j (P0-b) closed one half of the honesty gap: with no backend, nothing may
//! answer `DeviceConfirmed`. This module closes the other half: with a
//! backend, an action sentence may only come from the CORE, rendered from the
//! tool's structured fact after the backend read the state back.
//!
//! Why not let the model phrase it? The 2026-11-12 turn showed the failure
//! mode: a model truthfully relaying a lying receipt ("灯已经关好了"). The
//! model cannot be the authority for what happened to the physical world —
//! the receipt is. So the core speaks the fact sentence, and the model's
//! prose is only allowed to add non-action context. A model sentence that
//! asserts an action without a confirmed receipt is REFUSED (fail-closed):
//! it never reaches the speaker.
//!
//! Deliberately NOT here: a second model as an isolated reviewer (A's
//! `presentation.rs`). The cost/latency does not pay for itself when the core
//! already owns the sentence and the claim detector is deterministic.

use crate::tool::{Evidence, FactBasis, ToolFact, ToolResult, ToolStatus};

/// What a person hears when a model claims an action that no tool confirmed.
pub const UNSUPPORTED_ACTION_REFUSAL: &str = "我没有执行这个操作，也不能确认结果。";

/// True only when the tool's own fact AND the receipt agree that the target
/// state was observed. Two independent gates: a tool that forgets to set the
/// evidence still cannot fake `confirmed`.
pub fn is_confirmed(result: &ToolResult, fact: &ToolFact) -> bool {
    if !fact.confirmed || result.status != ToolStatus::Completed {
        return false;
    }
    match fact.basis {
        // A state read back by the backend. A transport write is not enough.
        FactBasis::ReadBack => {
            matches!(
                result.evidence,
                Evidence::DeviceConfirmed | Evidence::Observed
            )
        }
        // The device's own play-stage event for this play (`accepted` is the
        // device's word, not ours; the terminal confirmation is minutes away
        // for a song).
        FactBasis::DeviceStage => {
            matches!(result.evidence, Evidence::DeviceConfirmed | Evidence::Sent)
        }
        // R2-BM6: the device's own receipt carried the resulting value
        // (`volume_receipt`, `mode_report`). Only a device confirmation.
        FactBasis::DeviceReceipt => result.evidence == Evidence::DeviceConfirmed,
    }
}

/// Render the sentence the core owns. Every branch is honest about what was
/// observed: only `DeviceConfirmed` may use「已…」; accepted/submitted/unknown
/// say the outcome was not confirmed; failed says the action did not finish.
pub fn render_fact(result: &ToolResult, fact: &ToolFact) -> String {
    let subject = fact.subject.trim();
    let subject = if subject.is_empty() {
        "这个操作"
    } else {
        subject
    };
    let noun = action_noun(&fact.action);
    if is_confirmed(result, fact) {
        return match fact.action.as_str() {
            "on" | "open" => format!("{subject}已打开。"),
            "off" | "close" => format!("{subject}已关闭。"),
            "play" => format!("已开始播放{subject}。"),
            "pause" => format!("{subject}已暂停。"),
            "resume" => format!("{subject}已继续播放。"),
            "stop" => format!("{subject}已停止。"),
            "repeat" => match fact.value.as_deref().filter(|v| !v.trim().is_empty()) {
                Some(v) => format!("{subject}已设为{}。", v.trim()),
                None => format!("{subject}循环模式已设置。"),
            },
            "set" => match fact.value.as_deref().filter(|v| !v.trim().is_empty()) {
                Some(v) => format!("{subject}已调到{}。", v.trim()),
                None => format!("{subject}已设置。"),
            },
            "switch" => match fact.value.as_deref().filter(|v| !v.trim().is_empty()) {
                Some(v) => format!("已把{subject}切换到{}。", v.trim()),
                None => format!("{subject}已切换。"),
            },
            // A READING, not an action (2026-11-15): 「现在音量多少」 must answer
            // with the device's number, not with 「已完成」.
            "get" => match fact.value.as_deref().filter(|v| !v.trim().is_empty()) {
                Some(v) => format!("{subject}是{}。", v.trim()),
                None => format!("{subject}还读不到。"),
            },
            _ => format!("{subject}已完成（设备已确认）。"),
        };
    }
    // A reading that failed reads differently from an action that failed:
    // 「没能确认音量是否查询」 is not a sentence a person says.
    if fact.action == "get" {
        let detail = fact
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| clip(d, 40));
        return match result.status {
            ToolStatus::Failed => match detail {
                Some(d) => format!("没能读到{subject}：{d}。"),
                None => format!("没能读到{subject}。"),
            },
            ToolStatus::Cancelled => format!("{subject}的查询已经取消。"),
            _ => match detail {
                Some(d) => format!("没能读到{subject}：{d}。"),
                None => format!("没能读到{subject}。"),
            },
        };
    }
    match result.status {
        ToolStatus::Failed => {
            let detail = fact
                .detail
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(|d| clip(d, 40));
            match detail {
                Some(d) => format!("没有完成{subject}的{noun}：{d}。"),
                None => format!("没有完成{subject}的{noun}。"),
            }
        }
        ToolStatus::Cancelled => format!("{subject}的{noun}已经取消，没有执行。"),
        ToolStatus::Completed if result.evidence == Evidence::Sent => {
            format!("已发送请求，未验证最终状态（没能确认{subject}是否{noun}）。")
        }
        // Unknown / Partial / Completed-but-not-confirmed all mean the same
        // thing to a person: we do not know. Never dress this as success.
        _ => format!("没能确认{subject}是否{noun}。"),
    }
}

fn action_noun(action: &str) -> &'static str {
    match action {
        "on" | "open" => "打开",
        "off" | "close" => "关闭",
        "play" => "播放",
        "pause" => "暂停",
        "resume" => "继续播放",
        "stop" => "停止",
        "repeat" => "循环设置",
        "set" => "调节",
        "switch" => "切换",
        "get" => "查询",
        _ => "执行",
    }
}

fn clip(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Does `sentence` assert that a household/device/media ACTION happened?
///
/// This is the fail-closed gate, so it is deliberately lexical. Its bias is
/// asymmetric: a false positive drops a sentence the model did not need to
/// say (the core owns action facts anyway), while a false negative would let
/// an unbacked claim reach the speaker. The narrative guard exists because a
/// story ("他关上了门") must still be told: third-person/story subjects are
/// not claims about what THIS assistant did, unless the sentence also speaks
/// in the assistant's first/second person.
pub fn asserts_action(sentence: &str) -> bool {
    let s = sentence.trim();
    if s.is_empty() {
        return false;
    }
    // A third-person/story subject makes the WHOLE sentence narration, even
    // if one clause looks like a state change.
    let story_subject = ["他", "她", "它", "他们", "她们", "从前", "有一天", "故事"]
        .iter()
        .any(|p| s.starts_with(p));
    let first_person = ["我", "你", "您"].iter().any(|p| s.contains(p));
    if story_subject && !first_person {
        return false;
    }
    // Denials are scoped to their CLAUSE: 「灯已经关好了，没有其他事了」
    // still claims the light is off (audit Q1). A pure question clause is
    // skipped; an earlier claim clause is still checked.
    let clauses: Vec<&str> = s
        .split(['，', ',', '；', ';'])
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    for clause in clauses {
        let is_question = clause.contains('？')
            || clause.contains('?')
            || clause.ends_with('吗')
            || clause.ends_with('么')
            || clause.ends_with('呢');
        if is_question {
            continue;
        }
        if asserts_action_core(clause) {
            return true;
        }
    }
    false
}

fn asserts_action_core(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // Explicit denials / inability. Keep this list narrow: a bare "不" would
    // swallow "已经关好了，不用管" (a real claim followed by a dismissal).
    const DENIALS: [&str; 12] = [
        "没有", "没能", "还没", "尚未", "未", "无法", "不能", "不了", "不关", "不开", "不播", "别",
    ];
    if DENIALS.iter().any(|d| s.contains(d)) {
        return false;
    }
    let story_subject = ["他", "她", "它", "他们", "她们", "从前", "有一天", "故事"]
        .iter()
        .any(|p| s.starts_with(p));
    let first_person = ["我", "你", "您"].iter().any(|p| s.contains(p));
    if story_subject && !first_person {
        return false;
    }
    const VERBS: [&str; 29] = [
        "关闭",
        "打开",
        "关",
        "开",
        "播放",
        "放",
        "暂停",
        "继续",
        "停止",
        "启动",
        "调节",
        "调到",
        "调成",
        "设置",
        "设为",
        "切换",
        "搞定",
        "处理",
        "弄",
        "熄灭",
        "点亮",
        "关上",
        "开启",
        "响起",
        "亮",
        "灭",
        "播放起来",
        "放起来",
        "关闭了",
    ];
    const MARKERS: [&str; 24] = [
        "已经",
        "已",
        "好了",
        "完成",
        "成功",
        "正在",
        "关了",
        "开了",
        "掉了",
        "播放着",
        "搞定了",
        "弄好了",
        "设好了",
        "调好了",
        "开始播放",
        "开始放",
        "暂停了",
        "继续了",
        "停止了",
        "打开了",
        "灭了",
        "亮了",
        "起来了",
        "关了",
    ];
    if !VERBS.iter().any(|v| contains_action_verb(s, v)) || !MARKERS.iter().any(|m| s.contains(m)) {
        return false;
    }
    if is_playback_observation(s) {
        return false;
    }
    // Ownership: a sentence without the assistant's voice and without a
    // household/device noun is narration ("小红打开了门"), not a claim.
    const OWNERS: [&str; 12] = [
        "我", "你", "您", "已", "已经", "正在", "帮", "为", "给", "系统", "助手", "搞定",
    ];
    const THINGS: [&str; 13] = [
        "灯", "音乐", "歌", "播放", "音量", "空调", "窗帘", "电视", "设备", "房间", "设置", "暂停",
        "继续",
    ];
    OWNERS.iter().any(|o| s.contains(o)) || THINGS.iter().any(|t| s.contains(t))
}

/// Read-only playback status observations (answering "刚才在放什么" or describing
/// an already-paused session) are state descriptions, not unconfirmed action claims.
fn is_playback_observation(s: &str) -> bool {
    const AGENCY: [&str; 17] = [
        "我",
        "帮你",
        "帮您",
        "已帮",
        "已经帮",
        "为你",
        "为您",
        "给你",
        "给您",
        "替你",
        "替您",
        "搞定",
        "弄好",
        "设好",
        "调好",
        "成功",
        "完成",
    ];
    const DEVICE_MUTATIONS: [&str; 21] = [
        "关闭",
        "打开",
        "关上",
        "关掉",
        "开启",
        "熄灭",
        "点亮",
        "调到",
        "调成",
        "设为",
        "设置",
        "切换",
        "开始播放",
        "开始放",
        "放上",
        "放好",
        "播上",
        "放起来",
        "播放起来",
        "继续播放",
        "停止播放",
    ];
    if AGENCY.iter().any(|a| s.contains(a)) || DEVICE_MUTATIONS.iter().any(|m| s.contains(m)) {
        return false;
    }
    const PAST_RECALL: [&str; 7] = ["刚才", "刚刚", "方才", "之前", "先前", "原本", "原先"];
    const PLAYBACK_TERMS: [&str; 5] = ["播放", "放", "播报", "听", "暂停"];
    if PAST_RECALL.iter().any(|p| s.contains(p)) && PLAYBACK_TERMS.iter().any(|t| s.contains(t)) {
        return true;
    }
    const PAUSED_STATES: [&str; 16] = [
        "暂停在",
        "处于暂停",
        "暂停状态",
        "已暂停在",
        "目前暂停",
        "当前暂停",
        "现在暂停",
        "目前已暂停",
        "当前已暂停",
        "现在已暂停",
        "目前已经暂停",
        "当前已经暂停",
        "现在已经暂停",
        "唤醒时已暂停",
        "唤醒时已经暂停",
        "自动暂停",
    ];
    PAUSED_STATES.iter().any(|p| s.contains(p))
}

/// Two-character words that CONTAIN a single-character verb from [`VERBS`]
/// but are not household actions.
///
/// The gate is deliberately fail-closed, but a single Hanzi is a license to
/// match the wrong word: "关于英伟达，最近**已经**有新卡发布了" matched 关 +
/// 已经, so a query answer was suppressed and the refusal "我没有执行这个
/// 操作" was appended (2026-11-13, M7-2). The same held for 漂亮/放心/开心/
/// 开始. A single-character verb therefore only counts when it is not part of
/// one of these compounds.
const NON_ACTION_COMPOUNDS: [&str; 32] = [
    "关于", "关系", "关注", "关心", "关键", "机关", "有关", "无关", "开心", "开始", "开放", "离开",
    "公开", "开会", "展开", "开学", "开玩", "放心", "放弃", "放假", "放学", "放晴", "大放", "漂亮",
    "明亮", "天亮", "亮相", "月亮", "消灭", "歼灭", "弄清", "弄明",
];

/// Does `s` contain `verb` as an ACTION verb? Multi-character verbs are
/// unambiguous; a single Hanzi is checked against the non-action compounds at
/// every occurrence (so one action occurrence still wins).
fn contains_action_verb(s: &str, verb: &str) -> bool {
    if verb.chars().count() > 1 {
        return s.contains(verb);
    }
    let Some(c) = verb.chars().next() else {
        return false;
    };
    let mut from = 0usize;
    while let Some(pos) = s[from..].find(c) {
        let i = from + pos;
        let prev = s[..i].chars().next_back();
        let next = s[i + c.len_utf8()..].chars().next();
        let pair_next: String = [Some(c), next].into_iter().flatten().collect();
        let pair_prev: String = [prev, Some(c)].into_iter().flatten().collect();
        let swallowed = NON_ACTION_COMPOUNDS
            .iter()
            .any(|w| *w == pair_next || *w == pair_prev);
        if !swallowed {
            return true;
        }
        from = i + c.len_utf8();
        if from >= s.len() {
            break;
        }
    }
    false
}

/// Sanitize a WHOLE model answer that never streamed as deltas (some
/// providers only send the terminal content). Returns the text to speak and
/// how many sentences were suppressed.
pub fn sanitize_whole(text: &str, has_fact: bool) -> (String, usize) {
    sanitize_whole_with_prose(text, has_fact, false)
}

pub fn sanitize_whole_with_prose(
    text: &str,
    has_fact: bool,
    allow_followup_prose: bool,
) -> (String, usize) {
    let mut out = String::new();
    let mut suppressed = 0;
    let mut refusal_sent = false;
    let sentences = split_sentences(text);
    if has_fact && !allow_followup_prose {
        // Same enforced boundary as the streaming gate: the core owns the
        // action sentences, so no model prose is kept.
        return (String::new(), sentences.len());
    }
    for sentence in sentences {
        if asserts_action(&sentence) {
            suppressed += 1;
            if !has_fact && !refusal_sent {
                refusal_sent = true;
                out.push_str(UNSUPPORTED_ACTION_REFUSAL);
            }
            continue;
        }
        out.push_str(&sentence);
    }
    (out, suppressed)
}

/// Split on hard sentence boundaries, keeping the terminator with its
/// sentence. Empty pieces are dropped.
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if "。！？!?；;\n".contains(c) {
            let t = cur.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
            cur.clear();
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolError, ToolFact};

    fn result(status: ToolStatus, evidence: Evidence) -> ToolResult {
        ToolResult {
            status,
            evidence,
            result: None,
            error: None,
            fact: None,
        }
    }

    fn fact(action: &str, confirmed: bool) -> ToolFact {
        ToolFact {
            subject: "儿童房的灯".into(),
            action: action.into(),
            basis: FactBasis::ReadBack,
            confirmed,
            detail: None,
            value: None,
        }
    }

    #[test]
    fn a_confirmed_action_is_spoken_by_the_core() {
        let r = result(ToolStatus::Completed, Evidence::DeviceConfirmed);
        let f = fact("off", true);
        assert_eq!(render_fact(&r, &f), "儿童房的灯已关闭。");
        assert_eq!(render_fact(&r, &fact("on", true)), "儿童房的灯已打开。");
        let music = ToolFact {
            subject: "《夜曲》".into(),
            action: "play".into(),
            basis: FactBasis::DeviceStage,
            confirmed: true,
            detail: None,
            value: None,
        };
        assert_eq!(render_fact(&r, &music), "已开始播放《夜曲》。");
    }

    #[test]
    fn accepted_is_not_confirmed() {
        // The transport said 200, but nobody read the target state back.
        let r = result(ToolStatus::Completed, Evidence::Submitted);
        let f = fact("off", false);
        let s = render_fact(&r, &f);
        assert!(s.contains("没能确认"), "{s}");
        assert!(!s.contains("已关闭"), "{s}");
    }

    #[test]
    fn a_tool_cannot_fake_confirmation_without_the_receipt() {
        // fact.confirmed=true but evidence is only `Sent` -> not confirmed.
        let r = result(ToolStatus::Completed, Evidence::Sent);
        let s = render_fact(&r, &fact("off", true));
        assert!(s.contains("没能确认"), "{s}");
    }

    #[test]
    fn a_device_stage_receipt_may_back_a_play_fact() {
        // Music: the device itself reported `accepted` for this play. `Sent`
        // is a device event here, not our transport write.
        let mut f = ToolFact {
            subject: "《夜曲》".into(),
            action: "play".into(),
            basis: FactBasis::DeviceStage,
            confirmed: true,
            detail: None,
            value: None,
        };
        let r = result(ToolStatus::Completed, Evidence::Sent);
        assert!(is_confirmed(&r, &f));
        assert_eq!(render_fact(&r, &f), "已开始播放《夜曲》。");
        // The same evidence can never back a read-back fact.
        f.basis = FactBasis::ReadBack;
        assert!(!is_confirmed(&r, &f));
    }

    #[test]
    fn a_failure_is_named_and_never_dressed_as_success() {
        let mut r = result(ToolStatus::Failed, Evidence::Submitted);
        r.error = Some(ToolError {
            code: "HOME_ENTITY_NOT_ALLOWED".into(),
            retryable: false,
            detail: "这个实体不在白名单里".into(),
        });
        let mut f = fact("off", false);
        f.detail = Some("这个实体不在白名单里".into());
        let s = render_fact(&r, &f);
        assert!(s.starts_with("没有完成儿童房的灯的关闭"), "{s}");
        assert!(!s.contains("已关闭"), "{s}");
    }

    #[test]
    fn a_cancelled_action_says_so() {
        let r = result(ToolStatus::Cancelled, Evidence::Accepted);
        let s = render_fact(&r, &fact("off", false));
        assert!(s.contains("取消"), "{s}");
    }

    #[test]
    fn action_claims_are_detected_but_questions_and_denials_are_not() {
        for claim in [
            "灯已经关好了。",
            "已为您关闭儿童房的灯。",
            "儿童房的灯关掉了。",
            "正在播放《夜曲》。",
            "已开始播放。",
            "音乐暂停了。",
            "音量已经调到 80%。",
            "已经打开了。",
            "搞定了。",
            "设置完成。",
            "已经关好了，不用管。", // a real claim followed by a dismissal
            "灯已经关好了，还需要什么吗？", // claim + a trailing question
            "灯已经关好了，没有其他事了。", // audit Q1: denial in ANOTHER clause
            "灯已经熄灭了。",
            "灯亮了。",
            "音乐响起来了。",
            "灯关掉了。",
            "搞定了。",
            "设置完成。",
            "我已经帮你暂停了电台。",
            "刚才我已经把灯关好了。",
            "刚才已经放上了。",
            "刚才已经放起来了。",
            "刚才已经停止播放了。",
        ] {
            assert!(asserts_action(claim), "should be a claim: {claim}");
        }
        for harmless in [
            "灯已经关了吗？",
            "刚才在放「顶尖流行音乐台 Yes933」。",
            "刚才正在播放周杰伦的《晴天》。",
            "当前已暂停在电台「顶尖流行音乐台 Yes933」，没有正在外放的声音。",
            "目前已经暂停了。",
            "刚才因为唤醒已经暂停了。",
            "还没有接入家庭自动化，我现在开不了灯。",
            "没有关闭任何设备。",
            "要不要我帮你打开？",
            "关闭这个词是什么意思？",
            "我给你讲一个故事。",
            "好的。",
            "播放列表里没有这首歌。",
            // Narrative must survive the claim gate: a story is not a report
            // of what the assistant did.
            "从前有一个人，他关上了门。",
            "他关上了门。",
            "她把灯打开了。",
            "小红打开了门。",
            "故事里，音乐停了。",
            "他关上了门，灯关了。",
            "要不要我帮你打开？",
            "灯已经关了吗？",
            // M7-2 (2026-11-13): single Hanzi verbs matched inside unrelated
            // compounds, so query answers were suppressed and the refusal
            // 「我没有执行这个操作」 was appended to them.
            "关于英伟达，最近已经有新卡发布了。",
            "你放心，这项信息已经很清楚了。",
            "这篇报道写得漂亮，已经有不少人转发。",
            "我开始理解这个问题了。",
            "这个开关关系不大。",
        ] {
            assert!(
                !asserts_action(harmless),
                "should not be a claim: {harmless}"
            );
        }
    }

    #[test]
    fn a_device_receipt_backs_volume_and_mode_facts() {
        let r = result(ToolStatus::Completed, Evidence::DeviceConfirmed);
        let volume = ToolFact {
            subject: "客厅的音量".into(),
            action: "set".into(),
            basis: FactBasis::DeviceReceipt,
            confirmed: true,
            detail: None,
            value: Some("80%".into()),
        };
        assert!(is_confirmed(&r, &volume));
        assert_eq!(render_fact(&r, &volume), "客厅的音量已调到80%。");

        let mode = ToolFact {
            subject: "客厅的对话模式".into(),
            action: "switch".into(),
            basis: FactBasis::DeviceReceipt,
            confirmed: true,
            detail: None,
            value: Some("全双工".into()),
        };
        assert_eq!(render_fact(&r, &mode), "已把客厅的对话模式切换到全双工。");

        // A submitted request is not a receipt, even with the same fact.
        let submitted = result(ToolStatus::Completed, Evidence::Submitted);
        assert!(!is_confirmed(&submitted, &volume));
        assert_eq!(
            render_fact(&submitted, &volume),
            "没能确认客厅的音量是否调节。"
        );

        let failed = result(ToolStatus::Failed, Evidence::Accepted);
        let with_detail = ToolFact {
            detail: Some("设备只支持半双工".into()),
            ..mode.clone()
        };
        assert_eq!(
            render_fact(&failed, &with_detail),
            "没有完成客厅的对话模式的切换：设备只支持半双工。"
        );
    }

    #[test]
    fn a_whole_answer_is_sanitized_when_nothing_streamed() {
        let (out, n) = sanitize_whole("好的。灯已经关好了。还需要什么吗？", false);
        assert_eq!(n, 1);
        assert!(out.contains(UNSUPPORTED_ACTION_REFUSAL), "{out}");
        assert!(!out.contains("已关"), "{out}");
        assert!(out.contains("还需要什么吗"), "{out}");

        // With a core fact already said, the claim is simply dropped.
        let (out, n) = sanitize_whole("灯已经关好了。", true);
        assert_eq!(n, 1);
        assert_eq!(out, "");

        // Past-playback recall + paused-state tail must NOT append UNSUPPORTED_ACTION_REFUSAL.
        let (out, n) = sanitize_whole(
            "刚才正在播放电台「顶尖流行音乐台 Yes933」。目前已经暂停了。",
            false,
        );
        assert_eq!(n, 0, "read-only recall must not be suppressed: {out}");
        assert!(!out.contains(UNSUPPORTED_ACTION_REFUSAL), "{out}");
        assert!(
            out.contains("Yes933") && out.contains("目前已经暂停了"),
            "{out}"
        );
    }
}
