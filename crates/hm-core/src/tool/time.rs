//! Local clock tool (R1e, A's `clock.rs`): the time, date and weekday come
//! from THIS device's clock, never from the model's memory. A model that
//! answers "今天周几" from its training data is confidently wrong; a tool
//! result cannot be.
//!
//! Deliberately dependency-free: `localtime_r` through libc (already a
//! dependency) so the workspace does not grow a date library for one tool,
//! and the timezone is whatever the device is set to — the same truth the
//! user's wall clock shows.

use super::{SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use serde_json::{json, Value};

pub struct TimeTool;

/// One local clock reading, already broken into what answers need.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalNow {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    /// 0 = Sunday.
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl LocalNow {
    pub fn weekday_zh(&self) -> &'static str {
        match self.weekday {
            0 => "周日",
            1 => "周一",
            2 => "周二",
            3 => "周三",
            4 => "周四",
            5 => "周五",
            _ => "周六",
        }
    }
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
    pub fn clock(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
    pub fn iso(&self) -> String {
        format!("{}T{}:{:02}", self.date(), self.clock(), self.second)
    }
}

/// "10点52分" — the way a person says it (TTS reads "10:52" badly).
pub fn spoken_clock(now: &LocalNow) -> String {
    format!("{}点{:02}分", now.hour, now.minute)
}

/// RUI-local's usage (user direction 2026-11-12: "直接复用 rui local 的服务和用法"):
/// a PURE clock question is answered by the runtime, before the model runs.
/// The model used to be asked, and a weak model wrote the call as prose
/// ("[调用 time_now …]") and had it spoken aloud — the failure the user
/// reported. An exact-match table has no such failure mode, and the answer is
/// instant.
///
/// Only exact matches qualify: "今天周几，我要不要带伞" still goes to the model.
pub fn clock_reply(text: &str) -> Option<String> {
    let t = normalize_clock_text(text);
    if t.is_empty() || t.chars().count() > 12 {
        return None;
    }
    let now = local_now()?;
    // Time of day.
    let mut time_part = t.as_str();
    for p in ["现在", "当前"] {
        time_part = time_part.strip_prefix(p).unwrap_or(time_part);
    }
    time_part = time_part.strip_prefix('是').unwrap_or(time_part);
    time_part = time_part.strip_suffix('了').unwrap_or(time_part);
    if matches!(
        time_part,
        "几点" | "几点钟" | "什么时间" | "什么时刻" | "多少点"
    ) {
        return Some(format!("现在是{}。", spoken_clock(&now)));
    }
    // Date / weekday, with an optional day word. "现在几号" is as natural as
    // "今天几号" — the leading 现在 is not part of the day word.
    let mut date_part = t.as_str();
    for p in ["现在", "当前"] {
        date_part = date_part.strip_prefix(p).unwrap_or(date_part);
    }
    let (day_word, rest) = split_day_word(date_part)?;
    let offset = match day_word {
        "明天" => 1,
        "后天" => 2,
        "昨天" => -1,
        _ => 0,
    };
    let rest = rest.strip_prefix('是').unwrap_or(rest);
    let target = if offset == 0 {
        now
    } else {
        local_now_offset(offset)?
    };
    match rest {
        "星期几" | "周几" | "礼拜几" => Some(format!(
            "{}星期{}。",
            day_word_or_today(day_word),
            weekday_zh(target.weekday)
        )),
        "几号" | "多少号" | "几月几号" | "几月几日" | "什么日期" => Some(format!(
            "{}是{}月{}日。",
            day_word_or_today(day_word),
            target.month,
            target.day
        )),
        _ => None,
    }
}

fn day_word_or_today(word: &str) -> &str {
    if word.is_empty() {
        "今天"
    } else {
        word
    }
}

fn weekday_zh(weekday: u32) -> &'static str {
    match weekday {
        0 => "日",
        1 => "一",
        2 => "二",
        3 => "三",
        4 => "四",
        5 => "五",
        _ => "六",
    }
}

/// `(day word, remainder)`. The empty day word means "today".
fn split_day_word(t: &str) -> Option<(&str, &str)> {
    for w in ["今天", "明天", "后天", "昨天", ""] {
        if let Some(rest) = t.strip_prefix(w) {
            return Some((w, rest));
        }
    }
    Some(("", t))
}

/// Strip what a speaker adds around a short question: punctuation, spaces,
/// politeness prefixes and the assistant's own name (a barge-in transcript or
/// a wake-rest can carry it).
fn normalize_clock_text(text: &str) -> String {
    let mut s: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && !"，。？！、,.?!".contains(*c))
        .collect();
    for p in ["请问", "请告诉我", "告诉我", "我想知道", "你好"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.to_string();
        }
    }
    while let Some(last) = s.chars().last() {
        if "呀啊呢吧吗".contains(last) {
            s.pop();
        } else {
            break;
        }
    }
    s
}

/// The device runs in UTC (measured 2026-11-12: `date` = 02:35 GMT while the
/// user's clock said 10:35 CST), so localtime alone answered 8 hours in the
/// past. A POSIX TZ string is used instead of a zone name because musl has no
/// tzdata on this image: `CST-8` is UTC+8 without any database. `HM_TZ`
/// overrides it for another household.
const DEFAULT_TZ: &str = "CST-8";

fn ensure_timezone() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let tz = std::env::var("HM_TZ").unwrap_or_else(|_| DEFAULT_TZ.to_string());
        if let Ok(c) = std::ffi::CString::new(tz) {
            unsafe {
                libc::setenv(b"TZ\0".as_ptr() as *const libc::c_char, c.as_ptr(), 1);
                // libc does not bind tzset on every target; localtime_r
                // re-reads TZ on the next call, which is all this needs.
                extern "C" {
                    fn tzset();
                }
                tzset();
            }
        }
    });
}

/// Read the device's local clock. `None` only if the C library refuses the
/// conversion (never observed; a tool must not panic on a clock read).
pub fn local_now() -> Option<LocalNow> {
    ensure_timezone();
    unsafe { local_now_at(libc::time(std::ptr::null_mut())) }
}

/// The local clock `days` days from now (negative = earlier). Calendar
/// arithmetic on the device's own clock: the offline tool has no timezone
/// database to consult.
pub fn local_now_offset(days: i64) -> Option<LocalNow> {
    ensure_timezone();
    // `time_t` is i32 on armv7 (the device) and i64 on the host: do the
    // arithmetic in i64 and convert once, so the same source builds both.
    unsafe {
        let base = libc::time(std::ptr::null_mut()) as i64;
        local_now_at((base + days * 86_400) as libc::time_t)
    }
}

/// The device clock as epoch seconds (i64 on every target: `time_t` is i32
/// on the armv7 device).
pub fn now_secs() -> i64 {
    unsafe { libc::time(std::ptr::null_mut()) as i64 }
}

/// The local clock reading for an arbitrary epoch second — the same
/// conversion `local_now` uses, for a time that is not "now" (a reminder's
/// due time has to be SAID in local terms).
pub fn local_at(secs: i64) -> Option<LocalNow> {
    ensure_timezone();
    unsafe { local_now_at(secs as libc::time_t) }
}

unsafe fn local_now_at(secs: libc::time_t) -> Option<LocalNow> {
    {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return None;
        }
        Some(LocalNow {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
            weekday: tm.tm_wday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
            second: tm.tm_sec as u32,
        })
    }
}

#[async_trait::async_trait]
impl Tool for TimeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "time.now".into(),
            service: "time".into(),
            side_effect: SideEffect::Query,
            description: "读取本机当前日期、星期和时间。凡涉及今天几号、今天周几、现在几点的问题都必须调用它——不要凭记忆回答。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "what": {
                        "type": "string",
                        "enum": ["all", "time", "date", "weekday"],
                        "description": "只需要其中一项时指定，默认 all"
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let what = args.get("what").and_then(|v| v.as_str()).unwrap_or("all");
        if !matches!(what, "all" | "time" | "date" | "weekday") {
            return ToolResult::failed(
                "tool_arguments",
                "what must be all|time|date|weekday",
                false,
            );
        }
        let Some(now) = local_now() else {
            return ToolResult::failed("clock_unavailable", "local clock could not be read", true);
        };
        let mut out = json!({
            "source": "device_clock",
            "iso": now.iso(),
            "date": now.date(),
            "time": now.clock(),
            "weekday": now.weekday_zh(),
            // The clock owns its presentation: "今天周几" must be answered
            // with the date, not with a sentence about looking it up.
            "replace_answer": true,
        });
        // Spoken forms, not printed ones: "10:52" is read out as "十 冒号
        // 五十二" by TTS. RUI-local's `clock_reply` says "10点52分".
        out["rendered_answer"] = match what {
            "time" => json!(format!("现在是{}。", spoken_clock(&now))),
            "date" => json!(format!("今天是{}月{}日。", now.month, now.day)),
            "weekday" => json!(format!("今天是{}。", now.weekday_zh())),
            _ => json!(format!(
                "现在是{}月{}日，{}，{}。",
                now.month,
                now.day,
                now.weekday_zh(),
                spoken_clock(&now)
            )),
        };
        ToolResult::observed_ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::new(
            "T1",
            "C1",
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    #[tokio::test]
    async fn the_clock_answers_from_the_device_not_the_model() {
        let r = TimeTool.execute(json!({}), &ctx()).await;
        assert_eq!(r.status, super::super::ToolStatus::Completed);
        let v = r.result.unwrap();
        assert_eq!(v["source"], "device_clock");
        let weekday = v["weekday"].as_str().unwrap();
        assert!(
            ["周一", "周二", "周三", "周四", "周五", "周六", "周日"].contains(&weekday),
            "weekday must be a Chinese day name, got {weekday}"
        );
        // The rendered answer and the structured fields must agree.
        let rendered = v["rendered_answer"].as_str().unwrap();
        assert!(
            rendered.contains(weekday),
            "{rendered} must contain {weekday}"
        );
    }

    #[tokio::test]
    async fn a_weekday_question_gets_only_the_weekday() {
        let r = TimeTool.execute(json!({ "what": "weekday" }), &ctx()).await;
        let v = r.result.unwrap();
        let rendered = v["rendered_answer"].as_str().unwrap();
        assert!(rendered.starts_with("今天是周"), "got {rendered}");
        assert!(
            !rendered.contains(':'),
            "no clock in a weekday answer: {rendered}"
        );
    }

    #[tokio::test]
    async fn an_unknown_selector_is_refused_not_guessed() {
        let r = TimeTool.execute(json!({ "what": "maybe" }), &ctx()).await;
        assert_eq!(r.status, super::super::ToolStatus::Failed);
        assert_eq!(r.error.unwrap().code, "tool_arguments");
    }

    /// RUI-local's usage, pinned: the runtime answers these itself. The model
    /// is not asked, so it cannot narrate a call instead of making one.
    #[test]
    fn pure_clock_questions_are_answered_locally() {
        for q in [
            "几点了？",
            "几点了",
            "现在几点",
            "现在几点了",
            "几点钟了",
            "请问现在几点？",
            "现在是什么时间",
            "什么时间",
        ] {
            let a =
                super::clock_reply(q).unwrap_or_else(|| panic!("{q:?} must be answered locally"));
            assert!(
                a.starts_with("现在是") && a.contains('点') && a.ends_with('。'),
                "{q:?} -> {a}"
            );
        }
        let today = super::clock_reply("今天周几？").expect("weekday");
        assert!(
            today.starts_with("今天星期") && today.ends_with('。'),
            "{today}"
        );
        for q in [
            "周几",
            "今天星期几",
            "礼拜几",
            "今天几号",
            "今天是几月几号",
            "现在多少号",
        ] {
            assert!(
                super::clock_reply(q).is_some(),
                "{q:?} must be answered locally"
            );
        }
        // Tomorrow is computed from the device clock, not guessed.
        assert!(super::clock_reply("明天周几")
            .expect("tomorrow")
            .starts_with("明天星期"));
        assert!(super::clock_reply("后天几号")
            .expect("+2")
            .starts_with("后天是"));
        assert!(super::clock_reply("昨天几号")
            .expect("-1")
            .starts_with("昨天是"));
    }

    #[test]
    fn only_pure_questions_are_taken_from_the_model() {
        // Anything with a second question in it still needs the model.
        assert_eq!(super::clock_reply("今天周几，我要不要带伞"), None);
        assert_eq!(super::clock_reply("几点了我想出门买点东西"), None);
        assert_eq!(super::clock_reply("停"), None);
        assert_eq!(super::clock_reply("放首歌"), None);
        // The wake word (barge-in transcripts carry it) and politeness do not
        // defeat it.
        assert!(super::clock_reply("你好，今天周几？").is_some());
        assert!(super::clock_reply("请问今天周几呀").is_some());
    }

    #[test]
    fn a_reference_date_maps_to_the_right_weekday() {
        // 2026-11-12 is a Thursday; the formatter must say so.
        let now = LocalNow {
            year: 2026,
            month: 11,
            day: 12,
            weekday: 4,
            hour: 10,
            minute: 5,
            second: 0,
        };
        assert_eq!(now.weekday_zh(), "周四");
        assert_eq!(now.date(), "2026-11-12");
        assert_eq!(now.clock(), "10:05");
        assert_eq!(now.iso(), "2026-11-12T10:05:00");
    }
}
