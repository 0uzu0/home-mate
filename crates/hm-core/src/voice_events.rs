//! Persistent voice-event journal (S3/M6b, 2026-11-13).
//!
//! Why this exists: `VoicePipeline::note()` keeps the last 100 events in
//! memory so the console has a live feed, but acceptance needs to attribute
//! EVERY attempt in a 40-minute window. Ten wake tries already produce 60–100
//! events, so the ring is a snapshot, not a record. This module is the
//! bounded, exportable side channel; the ring keeps its 100 (the console's
//! wording does not change).
//!
//! Shape:
//! - one JSON value per line, the same payloads the ring holds, plus a
//!   process-lifetime `seq` (the exact export cursor — `ts_ms` can collide);
//! - files live in `<data_dir>/voice-events/voice-events-YYYYMMDD.jsonl`,
//!   rotated at midnight UTC and at `MAX_FILE_BYTES` (a `-NN` suffix keeps
//!   the name sort chronological);
//! - the total directory budget is `MAX_TOTAL_BYTES`; the oldest files are
//!   removed when a rotation would exceed it;
//! - a dedicated writer thread owns the disk. `note()` only enqueues — a slow
//!   eMMC must never delay the voice pipeline. If the queue is somehow full
//!   the drop is COUNTED and written as its own event, never silently lost;
//! - no fsync per event: a process crash loses nothing already written (page
//!   cache), a power cut can lose the tail. This is a diagnostic feed, not a
//!   ledger of record.
//!
//! `GET /api/v0/voice/events` reads the files back. The cursor is `(epoch,
//! seq)`: `seq` restarts with the process, so a cursor from a previous epoch
//! is ignored rather than skipping the new epoch's events.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// One file stops growing here and the next part starts.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// The whole `voice-events/` directory (all days) stays under this.
pub const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
/// Queue depth between the pipeline and the writer. Sized so even a stalled
/// eMMC has minutes of headroom at the observed event rate.
const QUEUE_DEPTH: usize = 16_384;
/// Re-scan the directory this often (in events) even without a rotation, so a
/// burst of same-day parts can never hide an over-budget directory.
const TOTAL_CHECK_EVERY: u64 = 512;
pub const EXPORT_DEFAULT_LIMIT: usize = 5_000;
pub const EXPORT_MAX_LIMIT: usize = 20_000;

const FILE_PREFIX: &str = "voice-events-";
const FILE_SUFFIX: &str = ".jsonl";

#[derive(Default)]
struct Counters {
    written: u64,
    dropped: u64,
    rotated: u64,
    pruned: u64,
}

/// The writer thread's private state: the open file and its size.
struct Writer {
    dir: PathBuf,
    open: Option<OpenFile>,
    counters: Arc<Mutex<Counters>>,
    since_total_check: u64,
}

struct OpenFile {
    name: String,
    file: std::fs::File,
    bytes: u64,
    ymd: u32,
}

enum JournalCmd {
    Append(Value),
    Clear(SyncSender<()>),
}

/// Handle held by the pipeline. `append` is the only hot-path method.
pub struct VoiceEventJournal {
    tx: SyncSender<JournalCmd>,
    dir: PathBuf,
    counters: Arc<Mutex<Counters>>,
    epoch: u64,
}

impl VoiceEventJournal {
    /// Start the journal in `<data_dir>/voice-events`. `None` when the
    /// directory cannot be created — the voice path must keep working
    /// without a diagnostic feed (the reason is logged, once).
    pub fn start(data_dir: &Path, epoch: u64) -> Option<Arc<Self>> {
        if std::env::var("HM_VOICE_EVENTS").ok().as_deref() == Some("0") {
            tracing::info!("voice event journal disabled by HM_VOICE_EVENTS=0");
            return None;
        }
        let dir = data_dir.join("voice-events");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e,
                "voice event journal unavailable — the in-memory ring is all we have");
            return None;
        }
        let counters = Arc::new(Mutex::new(Counters::default()));
        let (tx, rx) = sync_channel::<JournalCmd>(QUEUE_DEPTH);
        let writer = Writer {
            dir: dir.clone(),
            open: None,
            counters: counters.clone(),
            since_total_check: 0,
        };
        std::thread::Builder::new()
            .name("voice-events".into())
            .spawn(move || run_writer(writer, rx))
            .ok()?;
        Some(Arc::new(Self {
            tx,
            dir,
            counters,
            epoch,
        }))
    }

    /// Enqueue one event. Never blocks, never fails the caller.
    pub fn append(&self, event: Value) {
        match self.tx.try_send(JournalCmd::Append(event)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                if let Ok(mut c) = self.counters.lock() {
                    c.dropped += 1;
                }
            }
        }
    }

    /// Synchronously fence the writer thread, close any open file handle,
    /// remove `voice-events/` from disk, and reset journal counters.
    pub fn clear(&self) {
        let (ack_tx, ack_rx) = sync_channel(1);
        if self.tx.send(JournalCmd::Clear(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(2));
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn stats(&self) -> Value {
        let c = self
            .counters
            .lock()
            .map(|c| (c.written, c.dropped, c.rotated, c.pruned))
            .unwrap_or((0, 0, 0, 0));
        let (files, bytes) = scan_files(&self.dir);
        json!({
            "dir": self.dir.display().to_string(),
            "epoch": self.epoch,
            "written": c.0,
            "dropped": c.1,
            "rotated": c.2,
            "pruned": c.3,
            "files": files.len(),
            "bytes": bytes,
            "file_cap_bytes": MAX_FILE_BYTES,
            "total_cap_bytes": MAX_TOTAL_BYTES,
        })
    }

    /// Read the journal back. `since_ts` is EXCLUSIVE (events with
    /// `ts_ms > since_ts`). `since_seq` is the exact cursor and is honoured
    /// only when `since_epoch` equals this process's epoch — a sequence from
    /// another process would otherwise skip everything after the restart.
    pub fn export(
        &self,
        since_ts: u64,
        since_seq: Option<u64>,
        since_epoch: Option<u64>,
        limit: usize,
    ) -> Value {
        self.export_with_tail(since_ts, since_seq, since_epoch, limit, false)
    }

    pub fn export_with_tail(
        &self,
        since_ts: u64,
        since_seq: Option<u64>,
        since_epoch: Option<u64>,
        limit: usize,
        tail: bool,
    ) -> Value {
        let limit = limit.clamp(1, EXPORT_MAX_LIMIT);
        // The seq cursor is (epoch, seq): a sequence number only means
        // something inside the process that minted it. Without the epoch
        // check, an OLD epoch's seq=5000 would match a cursor of 4000 and the
        // caller would then skip the new epoch's 1..4000 entirely.
        let seq_cursor = match (since_seq, since_epoch) {
            (Some(seq), Some(epoch)) if epoch == self.epoch && seq > 0 => Some((epoch, seq)),
            (Some(seq), None) if seq > 0 => Some((self.epoch, seq)),
            _ => None,
        };
        let mut files = scan_files(&self.dir).0;
        files.sort_by_key(|n| journal_key(n));
        let mut events: std::collections::VecDeque<Value> = std::collections::VecDeque::new();
        let mut truncated = false;
        'files: for name in &files {
            let path = self.dir.join(name);
            // `read_to_string` would reject the WHOLE file if the writer was
            // killed in the middle of a multi-byte character; a lossy read
            // keeps every complete line and lets the torn tail fail parsing
            // on its own.
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    // A torn tail line (killed mid-write) is skipped, never
                    // allowed to poison the export.
                    continue;
                };
                let ts = v["ts_ms"].as_u64().unwrap_or(0);
                let seq = v["seq"].as_u64();
                let epoch = v["core_epoch"].as_u64();
                let after = match (seq_cursor, seq, epoch) {
                    (Some((cur_epoch, cur_seq)), Some(s), Some(e)) => e == cur_epoch && s > cur_seq,
                    (Some(_), _, _) => false,
                    _ => ts > since_ts,
                };
                if !after {
                    continue;
                }
                if tail {
                    if events.len() >= limit {
                        events.pop_front();
                        truncated = true;
                    }
                    events.push_back(v);
                } else {
                    if events.len() >= limit {
                        truncated = true;
                        break 'files;
                    }
                    events.push_back(v);
                }
            }
        }
        let events: Vec<Value> = events.into_iter().collect();
        let last = events.last();
        let next_seq = last.and_then(|v| v["seq"].as_u64());
        let next_ts = last.and_then(|v| v["ts_ms"].as_u64());
        json!({
            "events": events,
            "count": events.len(),
            "truncated": truncated,
            "tail": tail,
            "epoch": self.epoch,
            "since_ts": since_ts,
            "since_seq": seq_cursor,
            "next_seq": next_seq,
            "next_ts": next_ts,
            "files": files,
            "stats": self.stats(),
        })
    }
}

fn run_writer(mut w: Writer, rx: std::sync::mpsc::Receiver<JournalCmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            JournalCmd::Append(ev) => w.write_one(&ev),
            JournalCmd::Clear(ack) => {
                w.open = None;
                w.since_total_check = 0;
                if let Ok(mut c) = w.counters.lock() {
                    *c = Counters::default();
                }
                let _ = std::fs::remove_dir_all(&w.dir);
                let _ = ack.send(());
            }
        }
    }
}

impl Writer {
    fn write_one(&mut self, ev: &Value) {
        let mut line = match serde_json::to_vec(ev) {
            Ok(mut v) => {
                v.push(b'\n');
                v
            }
            Err(_) => {
                self.bump(|c| c.dropped += 1);
                return;
            }
        };
        // One event bigger than a whole file would blow both budgets and
        // could never be pruned (it IS the current file). Refuse it as a
        // counted drop BEFORE opening anything: no event is allowed to own
        // the disk, and a refused event must not even create a file.
        if line.len() as u64 > MAX_FILE_BYTES {
            self.bump(|c| c.dropped += 1);
            return;
        }
        let ts = ev["ts_ms"].as_u64().unwrap_or_else(crate::id::now_ms);
        let ymd = ymd_key(ts);
        if self.open.as_ref().map(|o| o.ymd != ymd).unwrap_or(true) {
            self.rotate(ymd);
        }
        // A write must never split across two files; check BEFORE writing.
        let need_rotate = self
            .open
            .as_ref()
            .map(|o| o.bytes > 0 && o.bytes + line.len() as u64 > MAX_FILE_BYTES)
            .unwrap_or(false);
        if need_rotate {
            self.rotate(ymd);
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.file.write_all(&line).is_err() {
            // A dead file handle (unlikely: append-only on eMMC) is dropped;
            // the next event reopens. Count it as a drop rather than pretend.
            self.open = None;
            self.bump(|c| c.dropped += 1);
            return;
        }
        open.bytes += line.len() as u64;
        line.clear();
        self.bump(|c| c.written += 1);
        self.since_total_check += 1;
        if self.since_total_check >= TOTAL_CHECK_EVERY {
            self.since_total_check = 0;
            self.enforce_total(0);
        }
    }

    fn bump(&self, f: impl FnOnce(&mut Counters)) {
        if let Ok(mut c) = self.counters.lock() {
            f(&mut c);
        }
    }

    fn rotate(&mut self, ymd: u32) {
        self.open = None;
        // Pick the next free part number for this day (01, 02, …).
        let mut part = 1u32;
        let name = loop {
            let candidate = if part == 1 {
                format!("{FILE_PREFIX}{ymd:08}{FILE_SUFFIX}")
            } else {
                format!("{FILE_PREFIX}{ymd:08}-{part:02}{FILE_SUFFIX}")
            };
            if !self.dir.join(&candidate).exists() {
                break candidate;
            }
            part += 1;
            if part > 99 {
                // 99 parts × 4 MiB is far beyond the total budget; the pruner
                // will have removed the oldest long before this.
                break format!("{FILE_PREFIX}{ymd:08}-99{FILE_SUFFIX}");
            }
        };
        let _ = std::fs::create_dir_all(&self.dir);
        let path = self.dir.join(&name);
        let existing = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => {
                self.open = Some(OpenFile {
                    name,
                    file,
                    bytes: existing,
                    ymd,
                });
                self.bump(|c| c.rotated += 1);
                self.enforce_total(1);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "voice event file open failed");
                self.open = None;
                self.bump(|c| c.dropped += 1);
            }
        }
    }

    /// Keep the directory under the total budget. The prune target reserves
    /// ONE file's worth of growth (`MAX_FILE_BYTES`): the open file can grow
    /// to its own cap between two prunes, so pruning to the bare total would
    /// let the directory sit at cap + 4 MiB. `keep` names the file just
    /// opened, which is never deleted.
    fn enforce_total(&mut self, keep: usize) {
        let target = MAX_TOTAL_BYTES.saturating_sub(MAX_FILE_BYTES);
        let (mut names, mut total) = scan_files(&self.dir);
        if total <= target {
            return;
        }
        names.sort_by_key(|n| journal_key(n));
        let current = self.open.as_ref().map(|o| o.name.clone());
        for name in names {
            if total <= target {
                break;
            }
            if Some(&name) == current.as_ref() {
                continue;
            }
            let _ = std::fs::create_dir_all(&self.dir);
            let path = self.dir.join(&name);
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
                self.bump(|c| c.pruned += 1);
                let _ = keep;
            }
        }
    }
}

/// Chronological sort key for a journal file name. **Do not sort raw names**:
/// `voice-events-20261113.jsonl` is part 1 and `voice-events-20261113-02.jsonl`
/// is part 2, but '-' sorts BEFORE '.', so raw name order puts part 2 first
/// and an export with a small `limit` advances its cursor past part 1's
/// events (found by the independent read, 2026-11-13).
fn journal_key(name: &str) -> (u32, u32) {
    let rest = name
        .strip_prefix(FILE_PREFIX)
        .unwrap_or(name)
        .strip_suffix(FILE_SUFFIX)
        .unwrap_or(name);
    let mut parts = rest.split('-');
    let ymd: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let part: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    (ymd, part)
}

/// File names in the journal directory and their total size, bytes only for
/// files this module owns.
fn scan_files(dir: &Path) -> (Vec<String>, u64) {
    let mut names = Vec::new();
    let mut total = 0u64;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (names, 0);
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(FILE_PREFIX) || !name.ends_with(FILE_SUFFIX) {
            continue;
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        total += len;
        names.push(name);
    }
    (names, total)
}

/// `YYYYMMDD` in UTC from a unix-ms timestamp (civil-from-days, no deps).
fn ymd_key(ms: u64) -> u32 {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    // A broken host clock must not panic the writer (its thread dying loses
    // every queued event): clamp to a representable calendar year.
    let year = year.clamp(1970, 9999) as u32;
    year * 10_000 + (m as u32) * 100 + d as u32
}

/// Process-lifetime event sequence. Module-level so every pipeline instance
/// in one process (tests) shares the cursor space; the epoch disambiguates.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Serializes sequence assignment WITH the enqueue. Two producers that took
/// seq 1 and 2 can otherwise reach the channel as 2 then 1, and an export
/// cursor at 2 would then exclude the late 1 forever (independent read,
/// 2026-11-13). The critical section is a counter increment and a channel
/// send — no disk I/O.
static SEQ_SEND_LOCK: Mutex<()> = Mutex::new(());

pub fn next_seq() -> u64 {
    EVENT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// M6b: stamp `event` with the next sequence and enqueue it atomically with
/// respect to other producers. Returns the event (the ring keeps the same
/// `seq` the file has). Safe to call when the journal is unavailable.
pub fn assign_seq_and_append(journal: Option<&VoiceEventJournal>, mut event: Value) -> Value {
    let _guard = SEQ_SEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let seq = EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    event["seq"] = json!(seq);
    if let Some(j) = journal {
        j.append(event.clone());
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hm-voice-events-{tag}-{}-{}",
            std::process::id(),
            crate::id::now_ms()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn ymd_is_civil_utc() {
        assert_eq!(ymd_key(0), 19700101);
        assert_eq!(ymd_key(86_400_000), 19700102);
        // 2026-11-13T00:00:00Z = 1_794_528_000 s; the last ms of the day
        // still belongs to it, the next one rolls over.
        let day = 1_794_528_000_000u64;
        assert_eq!(ymd_key(day), 20261113);
        assert_eq!(ymd_key(day + 86_399_999), 20261113);
        assert_eq!(ymd_key(day + 86_400_000), 20261114);
        // A leap day (2028-02-29 = 1_835_395_200 s).
        assert_eq!(ymd_key(1_835_395_200_000), 20280229);
    }

    #[test]
    fn journal_names_sort_chronologically() {
        // '-' sorts before '.', so raw name order would put part 2 first.
        assert_eq!(journal_key("voice-events-20261113.jsonl"), (20261113, 1));
        assert_eq!(journal_key("voice-events-20261113-02.jsonl"), (20261113, 2));
        assert!(
            journal_key("voice-events-20261113.jsonl")
                < journal_key("voice-events-20261113-02.jsonl")
        );
        assert!(
            journal_key("voice-events-20261113-02.jsonl")
                < journal_key("voice-events-20261114.jsonl")
        );
    }

    #[test]
    fn a_seq_cursor_only_matches_its_own_epoch() {
        let dir = tmpdir("epoch");
        let journal = dir.join("voice-events");
        std::fs::create_dir_all(&journal).unwrap();
        // A previous process (epoch 6) left high sequence numbers behind.
        let old: String = (100..103)
            .map(|seq| {
                format!("{{\"kind\":\"old\",\"seq\":{seq},\"core_epoch\":6,\"ts_ms\":1000}}\n")
            })
            .collect();
        std::fs::write(journal.join("voice-events-20261113.jsonl"), old).unwrap();
        // This process (epoch 7) starts its sequence from 1.
        let cur: String = (1..4)
            .map(|seq| {
                format!("{{\"kind\":\"cur\",\"seq\":{seq},\"core_epoch\":7,\"ts_ms\":2000}}\n")
            })
            .collect();
        std::fs::write(journal.join("voice-events-20261113-02.jsonl"), cur).unwrap();
        let j = VoiceEventJournal::start(&dir, 7).expect("journal");
        let out = j.export(0, Some(1), Some(7), 100);
        let events = out["events"].as_array().unwrap();
        assert!(
            events.iter().all(|e| e["core_epoch"] == json!(7)),
            "the old epoch's seq 100s must not answer a new-epoch cursor: {out}"
        );
        assert_eq!(events.len(), 2, "seq 2 and 3 of this epoch: {out}");
        assert_eq!(events[0]["seq"], json!(2));
        // A cursor from ANOTHER epoch is ignored and the ts filter applies.
        let other = j.export(1500, Some(100), Some(6), 100);
        assert_eq!(other["count"], json!(3), "{other}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_utf8_tail_keeps_every_complete_event() {
        let dir = tmpdir("torn");
        let journal = dir.join("voice-events");
        std::fs::create_dir_all(&journal).unwrap();
        let path = journal.join("voice-events-20261113.jsonl");
        let mut bytes = Vec::new();
        for seq in 1..=2 {
            bytes.extend_from_slice(
                format!("{{\"kind\":\"ok\",\"seq\":{seq},\"core_epoch\":5,\"ts_ms\":1000}}\n")
                    .as_bytes(),
            );
        }
        // Killed mid-character: the first two bytes of 中 (E4 B8) with no room
        // for the third. `read_to_string` would reject the whole file.
        bytes.extend_from_slice(b"{\"kind\":\"torn\",\"text\":\"\xe4\xb8");
        std::fs::write(&path, bytes).unwrap();
        let j = VoiceEventJournal::start(&dir, 5).expect("journal");
        let out = j.export(0, None, None, 100);
        assert_eq!(out["count"], json!(2), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_event_is_a_counted_drop() {
        let dir = tmpdir("oversize");
        let counters = Arc::new(Mutex::new(Counters::default()));
        let mut w = Writer {
            dir: dir.clone(),
            open: None,
            counters: counters.clone(),
            since_total_check: 0,
        };
        let big = json!({
            "kind": "huge", "seq": 1, "ts_ms": 1794577683870u64,
            "pad": "x".repeat(MAX_FILE_BYTES as usize + 16),
        });
        w.write_one(&big);
        let c = counters.lock().unwrap();
        assert_eq!(
            c.dropped, 1,
            "the oversized event must be counted as dropped"
        );
        assert_eq!(c.written, 0);
        drop(c);
        assert!(scan_files(&dir).0.is_empty(), "nothing may be written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absurd_timestamps_do_not_panic_the_writer() {
        // A host clock claiming year 584 billion must not overflow the date
        // key (and kill the writer thread with it).
        let key = ymd_key(u64::MAX);
        assert!(
            key >= 99_990_101 && key <= 99_991_231,
            "clamped to year 9999: {key}"
        );
    }

    #[test]
    fn concurrent_producers_keep_the_file_in_sequence_order() {
        let dir = tmpdir("order");
        let j = Arc::new(VoiceEventJournal::start(&dir, 3).expect("journal"));
        let mut handles = Vec::new();
        for t in 0..2 {
            let j = j.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..40 {
                    let ev = json!({
                        "kind": "probe", "ts_ms": 1794577683870u64, "t": t, "i": i
                    });
                    let _ = assign_seq_and_append(Some(&j), ev);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let out = j.export(0, None, None, 1000);
            let events = out["events"].as_array().cloned().unwrap_or_default();
            if events.len() == 80 {
                let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
                assert!(
                    seqs.windows(2).all(|w| w[0] < w[1]),
                    "file order must match the sequence: {seqs:?}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "writer never drained ({} events)",
                events.len()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_round_trip_with_seq_cursor() {
        let dir = tmpdir("roundtrip");
        let j = VoiceEventJournal::start(&dir, 7).expect("journal");
        for i in 0..3 {
            j.append(json!({
                "kind": "test", "ts_ms": 1_000 + i, "seq": next_seq(),
                // Real events always carry the process epoch; the seq cursor
                // is scoped to it.
                "core_epoch": 7,
            }));
        }
        // Wait for the writer to drain (bounded, so a slow machine cannot
        // hang the suite).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let out = j.export(0, None, None, 100);
            if out["count"].as_u64() == Some(3) {
                assert_eq!(out["truncated"], json!(false));
                let seq0 = out["events"][0]["seq"].as_u64().unwrap();
                let second = j.export(0, Some(seq0), Some(7), 100);
                assert_eq!(second["count"].as_u64(), Some(2));
                // A cursor from ANOTHER epoch is ignored (not used to skip).
                let other = j.export(0, Some(seq0), Some(6), 100);
                assert_eq!(other["count"].as_u64(), Some(3));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "writer never drained");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_and_total_budgets_are_enforced() {
        let dir = tmpdir("budget");
        let journal = dir.join("voice-events");
        std::fs::create_dir_all(&journal).unwrap();
        // Pre-seed the journal directory with an oversized "old" file: the
        // next rotation must prune it instead of growing without bound.
        let old = journal.join("voice-events-19700101.jsonl");
        std::fs::write(&old, vec![b'x'; (MAX_TOTAL_BYTES + 1) as usize]).unwrap();
        let j = VoiceEventJournal::start(&dir, 1).expect("journal");
        j.append(json!({ "kind": "test", "ts_ms": 1_700_000_000_000u64, "seq": next_seq() }));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (_, total) = scan_files(&journal);
            if total <= MAX_TOTAL_BYTES {
                assert!(!old.exists(), "the oldest file should be pruned");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "total budget never enforced: {total}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_with_tail_returns_newest_events_while_default_keeps_oldest() {
        let dir = tmpdir("tail");
        let j = VoiceEventJournal::start(&dir, 9).expect("journal");
        for i in 1..=5u64 {
            j.append(json!({
                "kind": "keyword_wake",
                "keyword": format!("w{i}"),
                "ts_ms": 1_700_000_000_000u64 + i * 1000,
                "seq": i,
                "core_epoch": 9,
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let all = j.export(0, None, None, 100);
            if all["count"].as_u64() == Some(5) {
                let head = j.export_with_tail(0, None, None, 2, false);
                assert_eq!(head["count"], json!(2));
                assert_eq!(head["truncated"], json!(true));
                assert_eq!(head["events"][0]["keyword"], json!("w1"));
                assert_eq!(head["events"][1]["keyword"], json!("w2"));

                let tail = j.export_with_tail(0, None, None, 2, true);
                assert_eq!(tail["count"], json!(2));
                assert_eq!(tail["truncated"], json!(true));
                assert_eq!(tail["tail"], json!(true));
                assert_eq!(tail["events"][0]["keyword"], json!("w4"));
                assert_eq!(tail["events"][1]["keyword"], json!("w5"));
                assert_eq!(tail["next_seq"], json!(5));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "writer never drained");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
