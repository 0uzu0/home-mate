//! Trace recording: every turn's observable steps, persisted, so the
//! console can answer "why did this request succeed or fail".

use crate::id;
use crate::store::{Result, Store};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct TraceRecorder {
    store: Arc<Store>,
}

#[derive(Clone)]
pub struct TraceHandle {
    pub trace_id: String,
    pub session_id: String,
    pub task_id: String,
    pub source: String,
    pub input_text: String,
    pub started_ms: u64,
    pub goal: String,
}

impl TraceRecorder {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn start(
        &self,
        session_id: &str,
        task_id: &str,
        source: &str,
        input_text: &str,
    ) -> Result<TraceHandle> {
        self.start_with_wake(session_id, task_id, source, input_text, None, None)
    }

    pub fn start_with_wake(
        &self,
        session_id: &str,
        task_id: &str,
        source: &str,
        input_text: &str,
        wake_channel: Option<&str>,
        wake_word: Option<&str>,
    ) -> Result<TraceHandle> {
        let h = TraceHandle {
            trace_id: id::next("R"),
            session_id: session_id.into(),
            task_id: task_id.into(),
            source: source.into(),
            input_text: input_text.into(),
            started_ms: id::now_ms(),
            goal: String::new(),
        };
        self.store.with(|c| {
            c.execute(
                "INSERT INTO traces (id, session_id, task_id, source, input_text, goal, status, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, '', 'running', ?6)",
                rusqlite::params![h.trace_id, h.session_id, h.task_id, h.source, h.input_text, h.started_ms as i64],
            )?;
            Ok(())
        })?;
        let mut start_payload = json!({ "source": h.source, "text": h.input_text });
        if let Some(ch) = wake_channel.filter(|s| !s.is_empty()) {
            start_payload["wake_channel"] = json!(ch);
        }
        if let Some(word) = wake_word.filter(|s| !s.is_empty()) {
            start_payload["wake_word"] = json!(word);
        }
        self.event(&h, "turn_start", start_payload);
        Ok(h)
    }

    pub fn event(&self, h: &TraceHandle, kind: &str, payload: Value) {
        if let Err(e) = self.try_event(h, kind, payload) {
            tracing::warn!(trace = %h.trace_id, kind, error = %e, "trace event dropped");
        }
    }

    fn try_event(&self, h: &TraceHandle, kind: &str, payload: Value) -> Result<()> {
        self.store.with(|c| {
            let seq: i64 = c.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM trace_events WHERE trace_id = ?1",
                [&h.trace_id],
                |r| r.get(0),
            )?;
            c.execute(
                "INSERT INTO trace_events (trace_id, seq, kind, payload, ts_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![h.trace_id, seq, kind, serde_json::to_string(&payload)?, id::now_ms() as i64],
            )?;
            Ok(())
        })
    }

    pub fn finish(&self, h: &TraceHandle, status: &str, goal: &str) {
        let duration = id::now_ms().saturating_sub(h.started_ms);
        if let Err(e) = self.store.with(|c| {
            c.execute(
                "UPDATE traces SET status=?2, goal=?3, duration_ms=?4 WHERE id=?1",
                rusqlite::params![h.trace_id, status, goal, duration as i64],
            )?;
            Ok(())
        }) {
            tracing::warn!(trace = %h.trace_id, error = %e, "trace finish dropped");
        }
    }

    /// Record a completed local fast-path reply (e.g. local clock `"今天周几"` /
    /// `"几点了"`) so it appears in Diagnostics traces with wake attribution.
    pub fn record_local_fast_path(
        &self,
        session_id: &str,
        source: &str,
        input_text: &str,
        answer: &str,
        wake_channel: Option<&str>,
        wake_word: Option<&str>,
    ) {
        let task_id = id::next("Tlocal");
        if let Ok(h) = self.start_with_wake(
            session_id,
            &task_id,
            source,
            input_text,
            wake_channel,
            wake_word,
        ) {
            self.event(&h, "local_fast_path", json!({ "answer": answer }));
            self.finish(&h, "completed", input_text);
        }
    }

    pub fn list(&self, limit: u32) -> Result<Vec<Value>> {
        self.store.with(|c| {
            let mut stmt = c.prepare(
                "SELECT t.id, t.session_id, t.task_id, t.source, t.input_text, t.goal, t.status, t.created_ms, t.duration_ms,
                        (SELECT e.payload FROM trace_events e WHERE e.trace_id = t.id AND e.kind = 'turn_start' ORDER BY e.seq ASC LIMIT 1)
                 FROM traces t ORDER BY t.created_ms DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map([limit], |r| {
                    let start_payload = r
                        .get::<_, Option<String>>(9)?
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                        .unwrap_or(Value::Null);
                    Ok(json!({
                        "id": r.get::<_, String>(0)?, "session_id": r.get::<_, String>(1)?,
                        "task_id": r.get::<_, String>(2)?, "source": r.get::<_, String>(3)?,
                        "input_text": r.get::<_, String>(4)?, "goal": r.get::<_, String>(5)?,
                        "status": r.get::<_, String>(6)?, "created_ms": r.get::<_, i64>(7)?,
                        "duration_ms": r.get::<_, i64>(8)?,
                        "wake_channel": start_payload.get("wake_channel").cloned().unwrap_or(Value::Null),
                        "wake_word": start_payload.get("wake_word").cloned().unwrap_or(Value::Null),
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn detail(&self, trace_id: &str) -> Result<Option<Value>> {
        self.store.with(|c| {
            let Some(meta) = (|| {
                c.query_row(
                    "SELECT id, session_id, task_id, source, input_text, goal, status, created_ms, duration_ms
                     FROM traces WHERE id=?1",
                    [trace_id],
                    |r| {
                        Ok(json!({
                            "id": r.get::<_, String>(0)?, "session_id": r.get::<_, String>(1)?,
                            "task_id": r.get::<_, String>(2)?, "source": r.get::<_, String>(3)?,
                            "input_text": r.get::<_, String>(4)?, "goal": r.get::<_, String>(5)?,
                            "status": r.get::<_, String>(6)?, "created_ms": r.get::<_, i64>(7)?,
                            "duration_ms": r.get::<_, i64>(8)?,
                        }))
                    },
                )
                .ok()
            })() else {
                return Ok(None);
            };
            let mut stmt = c.prepare(
                "SELECT seq, kind, payload, ts_ms FROM trace_events WHERE trace_id=?1 ORDER BY seq",
            )?;
            let events = stmt
                .query_map([trace_id], |r| {
                    Ok(json!({
                        "seq": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?,
                        "payload": serde_json::from_str::<Value>(&r.get::<_, String>(2)?)
                            .unwrap_or(json!({})),
                        "ts_ms": r.get::<_, i64>(3)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut v = meta;
            if let Some(start) = events.iter().find(|e| e["kind"] == "turn_start") {
                v["wake_channel"] = start["payload"].get("wake_channel").cloned().unwrap_or(Value::Null);
                v["wake_word"] = start["payload"].get("wake_word").cloned().unwrap_or(Value::Null);
            }
            v["events"] = Value::Array(events);
            Ok(Some(v))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_sequenced_and_retrievable() {
        let r = TraceRecorder::new(Arc::new(Store::open_in_memory().unwrap()));
        let h = r.start("S1", "T1", "text", "放首歌").unwrap();
        r.event(&h, "llm_start", json!({ "call": 1 }));
        r.event(&h, "tool_start", json!({ "tool": "music.play" }));
        r.finish(&h, "completed", "播放音乐");
        let d = r.detail(&h.trace_id).unwrap().unwrap();
        assert_eq!(d["events"].as_array().unwrap().len(), 3);
        assert_eq!(d["status"], "completed");
        assert_eq!(r.list(10).unwrap().len(), 1);
    }

    #[test]
    fn start_with_wake_persists_channel_and_word() {
        let r = TraceRecorder::new(Arc::new(Store::open_in_memory().unwrap()));
        let h = r
            .start_with_wake(
                "S2",
                "T2",
                "device:asr",
                "今天天气",
                Some("kws"),
                Some("你好小智"),
            )
            .unwrap();
        r.finish(&h, "completed", "晴");
        let rows = r.list(10).unwrap();
        assert_eq!(rows[0]["wake_channel"], "kws");
        assert_eq!(rows[0]["wake_word"], "你好小智");
        let d = r.detail(&h.trace_id).unwrap().unwrap();
        assert_eq!(d["wake_channel"], "kws");
        assert_eq!(d["wake_word"], "你好小智");
    }

    #[test]
    fn record_local_fast_path_persists_completed_trace_with_wake_attribution() {
        let r = TraceRecorder::new(Arc::new(Store::open_in_memory().unwrap()));
        r.record_local_fast_path(
            "dev:dot2-d",
            "device:asr",
            "今天周几",
            "今天是星期四。",
            Some("kws"),
            Some("你好小智"),
        );
        let rows = r.list(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"], "completed");
        assert_eq!(rows[0]["input_text"], "今天周几");
        assert_eq!(rows[0]["wake_channel"], "kws");
        assert_eq!(rows[0]["wake_word"], "你好小智");
        let id = rows[0]["id"].as_str().unwrap();
        let d = r.detail(id).unwrap().unwrap();
        let evs = d["events"].as_array().unwrap();
        assert!(evs
            .iter()
            .any(|e| e["kind"] == "local_fast_path" && e["payload"]["answer"] == "今天是星期四。"));
    }
}
