//! Reminders (2026-11-14): 「十分钟后提醒我关火」/「明天早上八点叫我起床」.
//!
//! State ownership: a reminder belongs to the DEVICE whose turn created it
//! (M12a decision: the speaker that was told is the speaker that reminds;
//! until the turn carries a device identity, that is the household's single
//! speaker). The store is the only truth — the voice tool, the management API
//! and the firing sweep all go through this service, so a reminder set by
//! voice is visible and cancellable in the console, and one cancelled there
//! never fires.
//!
//! Time math is done HERE, not by the model: the person says a shape
//! (「十分钟后」/「明天早上八点」), the model extracts the structure
//! (minutes, or day offset + hour + minute), and this module resolves it
//! against the DEVICE's own clock. A model that converts 「今晚八点」 with a
//! stale idea of "now" is confidently wrong by hours; a store row with the
//! computed `due_ms` is checkable. Local time follows the same rule the
//! clock tool uses (`CST-8` unless `HM_TZ`); a fixed offset has no DST
//! seam, so day arithmetic is exact.
//!
//! Firing is best-effort speech with a bounded retry (the sweep owns it);
//! this module only decides WHAT is due and records WHAT was done.

use crate::store::Store;
use crate::tool::time::{local_at, now_secs, LocalNow};
use serde::Serialize;
use std::sync::Arc;

pub const MAX_NOTE_CHARS: usize = 40;
pub const MAX_DELAY_MINUTES: u64 = 10_080; // a week
pub const MAX_DAY_OFFSET: u64 = 365;
pub const MAX_PENDING_PER_DEVICE: usize = 20;

/// Error codes a tool or the console can name. `detail` carries the
/// person-readable sentence.
#[derive(Debug, thiserror::Error)]
pub enum ReminderError {
    #[error("REMINDER_BAD_ARGS: {0}")]
    BadArgs(String),
    #[error("REMINDER_TIME_IN_PAST: {0}")]
    TimeInPast(String),
    #[error("REMINDER_LIMIT: {0}")]
    Limit(String),
    #[error("REMINDER_NOT_FOUND: {0}")]
    NotFound(String),
    #[error("REMINDER_STORE: {0}")]
    Store(String),
}

/// What the person asked for, already extracted from their words.
#[derive(Debug, Clone, PartialEq)]
pub enum When {
    /// 「十分钟后」「半小时后」「两天后」.
    DelayMinutes(u64),
    /// 「明天早上八点」. `day_offset` None = the person did not name a day:
    /// a time already past today rolls to tomorrow (「八点」 said at 21:00
    /// means tomorrow); an EXPLICIT day (今天) that is past is an error the
    /// person must resolve.
    At {
        day_offset: Option<u64>,
        hour: u32,
        minute: u32,
    },
}

#[derive(Debug, Clone)]
pub struct Reminder {
    pub id: i64,
    pub device: String,
    pub due_ms: u64,
    pub note: String,
    pub created_ms: u64,
    pub fired_ms: Option<u64>,
    pub cancelled_ms: Option<u64>,
}

/// The wire shape carries the derived `state` too: the console and the tests
/// read one word instead of re-deriving it from two nullable stamps.
impl Serialize for Reminder {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("Reminder", 8)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("device", &self.device)?;
        s.serialize_field("due_ms", &self.due_ms)?;
        s.serialize_field("note", &self.note)?;
        s.serialize_field("created_ms", &self.created_ms)?;
        s.serialize_field("fired_ms", &self.fired_ms)?;
        s.serialize_field("cancelled_ms", &self.cancelled_ms)?;
        s.serialize_field("state", self.state())?;
        s.end()
    }
}

impl Reminder {
    pub fn state(&self) -> &'static str {
        if self.cancelled_ms.is_some() {
            "cancelled"
        } else if self.fired_ms.is_some() {
            "fired"
        } else {
            "pending"
        }
    }
}

/// Resolve a shape against the device's own clock. Pure: `now_secs` and
/// `now` are inputs so a test can pin the clock.
pub fn resolve_due(spec: &When, now_secs: i64, now: &LocalNow) -> Result<u64, ReminderError> {
    match spec {
        When::DelayMinutes(minutes) => {
            if *minutes == 0 || *minutes > MAX_DELAY_MINUTES {
                return Err(ReminderError::BadArgs(format!(
                    "delay_minutes 必须在 1–{}",
                    MAX_DELAY_MINUTES
                )));
            }
            Ok((now_secs + (*minutes as i64) * 60) as u64 * 1000)
        }
        When::At {
            day_offset,
            hour,
            minute,
        } => {
            if *hour > 23 || *minute > 59 {
                return Err(ReminderError::BadArgs("hour 0–23，minute 0–59".into()));
            }
            // Seconds since LOCAL midnight, from the device's own reading.
            let into_day = (now.hour as i64) * 3600 + (now.minute as i64) * 60 + now.second as i64;
            let midnight = now_secs - into_day;
            let target_secs_of_day = (*hour as i64) * 3600 + (*minute as i64) * 60;
            let offset = match day_offset {
                // No day named: today while it is still ahead, else tomorrow.
                None => {
                    if target_secs_of_day > into_day {
                        0
                    } else {
                        1
                    }
                }
                Some(d) => {
                    if *d > MAX_DAY_OFFSET {
                        return Err(ReminderError::BadArgs(format!(
                            "day_offset 最多 {} 天",
                            MAX_DAY_OFFSET
                        )));
                    }
                    let due = midnight + (*d as i64) * 86_400 + target_secs_of_day;
                    if due <= now_secs {
                        return Err(ReminderError::TimeInPast(
                            "这一天的这个时间已经过去了".into(),
                        ));
                    }
                    *d as i64
                }
            };
            let due = midnight + offset * 86_400 + target_secs_of_day;
            if due <= now_secs {
                return Err(ReminderError::TimeInPast("这个时间已经过去了".into()));
            }
            Ok(due as u64 * 1000)
        }
    }
}

/// 「明天8点00分」 / 「11月16日8点00分」 — the way a person says it, computed
/// from the due time against the person's today.
pub fn spoken_when(due_secs: i64, now: &LocalNow) -> String {
    let Some(due) = local_at(due_secs) else {
        return String::new();
    };
    let day = if due.date() == now.date() {
        "今天"
    } else {
        let Some(tomorrow) = local_at(now_secs() + 86_400) else {
            return format!("{}月{}日", due.month, due.day);
        };
        if due.date() == tomorrow.date() {
            "明天"
        } else {
            let Some(after) = local_at(now_secs() + 2 * 86_400) else {
                return format!("{}月{}日", due.month, due.day);
            };
            if due.date() == after.date() {
                "后天"
            } else {
                &format!("{}月{}日", due.month, due.day)
            }
        }
    };
    format!("{day}{}点{:02}分", due.hour, due.minute)
}

pub struct ReminderService {
    store: Arc<Store>,
}

impl ReminderService {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Reminder> {
        Ok(Reminder {
            id: r.get("id")?,
            device: r.get("device")?,
            due_ms: r.get::<_, i64>("due_ms")? as u64,
            note: r.get("note")?,
            created_ms: r.get::<_, i64>("created_ms")? as u64,
            fired_ms: r.get::<_, Option<i64>>("fired_ms")?.map(|v| v as u64),
            cancelled_ms: r.get::<_, Option<i64>>("cancelled_ms")?.map(|v| v as u64),
        })
    }

    /// Create a pending reminder. The store row read back IS the receipt:
    /// the caller speaks from what this returns, never from the request.
    pub fn create(
        &self,
        device: &str,
        due_ms: u64,
        note: &str,
        now_ms: u64,
    ) -> Result<Reminder, ReminderError> {
        let note = note.trim();
        if note.is_empty() {
            return Err(ReminderError::BadArgs("note 不能为空".into()));
        }
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(ReminderError::BadArgs(format!(
                "note 最多 {MAX_NOTE_CHARS} 字"
            )));
        }
        if due_ms <= now_ms {
            return Err(ReminderError::TimeInPast("提醒时间已过去".into()));
        }
        let pending = self.pending_count(device)?;
        if pending >= MAX_PENDING_PER_DEVICE {
            return Err(ReminderError::Limit(format!(
                "这台音箱已有 {MAX_PENDING_PER_DEVICE} 个未提醒的提醒，先取消一些"
            )));
        }
        self.store
            .with(|c| {
                c.execute(
                    "INSERT INTO reminders (device, due_ms, note, created_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![device, due_ms as i64, note, now_ms as i64],
                )
                .map_err(crate::store::StoreError::Sql)?;
                Ok(c.last_insert_rowid())
            })
            .map_err(|e| ReminderError::Store(e.to_string()))
            .and_then(|id| {
                Ok(Reminder {
                    id,
                    device: device.to_string(),
                    due_ms,
                    note: note.to_string(),
                    created_ms: now_ms,
                    fired_ms: None,
                    cancelled_ms: None,
                })
            })
    }

    fn pending_count(&self, device: &str) -> Result<usize, ReminderError> {
        self.store
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM reminders
                     WHERE device = ?1 AND fired_ms IS NULL AND cancelled_ms IS NULL",
                    rusqlite::params![device],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(crate::store::StoreError::Sql)
                .map(|n| n as usize)
            })
            .map_err(|e| ReminderError::Store(e.to_string()))
    }

    /// Pending reminders, soonest first. `device: None` = the whole household.
    pub fn pending(&self, device: Option<&str>) -> Result<Vec<Reminder>, ReminderError> {
        self.store
            .with(|c| {
                let sql = "SELECT id, device, due_ms, note, created_ms, fired_ms, cancelled_ms
                           FROM reminders
                           WHERE fired_ms IS NULL AND cancelled_ms IS NULL
                           ORDER BY due_ms ASC";
                let mut stmt = c.prepare(sql).map_err(crate::store::StoreError::Sql)?;
                let rows = stmt
                    .query_map([], Self::row)
                    .map_err(crate::store::StoreError::Sql)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(crate::store::StoreError::Sql)?;
                Ok(rows
                    .into_iter()
                    .filter(|r| device.is_none_or(|d| r.device == d))
                    .collect())
            })
            .map_err(|e| ReminderError::Store(e.to_string()))
    }

    /// Everything, newest first — the console's view (pending + done).
    pub fn recent(&self, limit: usize) -> Result<Vec<Reminder>, ReminderError> {
        self.store
            .with(|c| {
                let mut stmt = c
                    .prepare(
                        "SELECT id, device, due_ms, note, created_ms, fired_ms, cancelled_ms
                         FROM reminders ORDER BY id DESC LIMIT ?1",
                    )
                    .map_err(crate::store::StoreError::Sql)?;
                let rows = stmt
                    .query_map([limit as i64], Self::row)
                    .map_err(crate::store::StoreError::Sql)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(crate::store::StoreError::Sql)?;
                Ok(rows)
            })
            .map_err(|e| ReminderError::Store(e.to_string()))
    }

    /// Due right now (pending only), soonest first.
    pub fn due(&self, now_ms: u64) -> Result<Vec<Reminder>, ReminderError> {
        self.store
            .with(|c| {
                let mut stmt = c
                    .prepare(
                        "SELECT id, device, due_ms, note, created_ms, fired_ms, cancelled_ms
                         FROM reminders
                         WHERE fired_ms IS NULL AND cancelled_ms IS NULL AND due_ms <= ?1
                         ORDER BY due_ms ASC",
                    )
                    .map_err(crate::store::StoreError::Sql)?;
                let rows = stmt
                    .query_map([now_ms as i64], Self::row)
                    .map_err(crate::store::StoreError::Sql)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(crate::store::StoreError::Sql)?;
                Ok(rows)
            })
            .map_err(|e| ReminderError::Store(e.to_string()))
    }

    /// Cancel one pending reminder. Cancelling an already-fired or unknown id
    /// is a NotFound the caller must name — not a silent no-op.
    pub fn cancel(&self, id: i64, now_ms: u64) -> Result<Reminder, ReminderError> {
        let changed = self
            .store
            .with(|c| {
                c.execute(
                    "UPDATE reminders SET cancelled_ms = ?2
                     WHERE id = ?1 AND fired_ms IS NULL AND cancelled_ms IS NULL",
                    rusqlite::params![id, now_ms as i64],
                )
                .map_err(crate::store::StoreError::Sql)
            })
            .map_err(|e| ReminderError::Store(e.to_string()))?;
        if changed == 0 {
            return Err(ReminderError::NotFound(format!("没有待提醒的提醒 #{id}")));
        }
        self.get(id)
            .ok_or_else(|| ReminderError::NotFound(format!("提醒 #{id} 不见了")))
    }

    fn get(&self, id: i64) -> Option<Reminder> {
        self.store
            .with(|c| {
                c.query_row(
                    "SELECT id, device, due_ms, note, created_ms, fired_ms, cancelled_ms
                     FROM reminders WHERE id = ?1",
                    [id],
                    Self::row,
                )
                .map_err(crate::store::StoreError::Sql)
            })
            .ok()
    }

    /// Mark fired. The sweep claims BEFORE speaking (at-most-once); a speech
    /// failure is a retry decision for the sweep, not a store rollback.
    pub fn mark_fired(&self, id: i64, now_ms: u64) -> Result<bool, ReminderError> {
        self.store
            .with(|c| {
                let changed = c
                    .execute(
                        "UPDATE reminders SET fired_ms = ?2
                         WHERE id = ?1 AND fired_ms IS NULL AND cancelled_ms IS NULL",
                        rusqlite::params![id, now_ms as i64],
                    )
                    .map_err(crate::store::StoreError::Sql)?;
                Ok(changed == 1)
            })
            .map_err(|e| ReminderError::Store(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TODAY at 16:00:00 local, from the real device clock. The clock value
    /// must be consistent with the timezone `spoken_when` converts in, so the
    /// tests stay correct on any day they run.
    fn pinned() -> (i64, LocalNow) {
        let real = crate::tool::time::now_secs();
        let rn = crate::tool::time::local_now().expect("device clock reads");
        let into_day = (rn.hour as i64) * 3600 + (rn.minute as i64) * 60 + rn.second as i64;
        let secs = real - into_day + 16 * 3600;
        let date = crate::tool::time::local_at(secs).expect("device clock reads");
        (
            secs,
            LocalNow {
                year: date.year,
                month: date.month,
                day: date.day,
                weekday: date.weekday,
                hour: 16,
                minute: 0,
                second: 0,
            },
        )
    }

    #[test]
    fn a_delay_resolves_from_the_device_clock() {
        let (now_secs, now) = pinned();
        let due = resolve_due(&When::DelayMinutes(10), now_secs, &now).unwrap();
        assert_eq!(due, (now_secs + 600) as u64 * 1000);
    }

    #[test]
    fn an_unnamed_time_that_already_passed_today_rolls_to_tomorrow() {
        let (now_secs, now) = pinned();
        let due = resolve_due(
            &When::At {
                day_offset: None,
                hour: 8,
                minute: 0,
            },
            now_secs,
            &now,
        )
        .unwrap();
        // 16:00 now, so 8:00 means TOMORROW 8:00.
        assert_eq!(due, (now_secs + 86_400 - 8 * 3600) as u64 * 1000);
    }

    #[test]
    fn an_explicit_today_in_the_past_is_refused_not_rolled() {
        let (now_secs, now) = pinned();
        let err = resolve_due(
            &When::At {
                day_offset: Some(0),
                hour: 8,
                minute: 0,
            },
            now_secs,
            &now,
        )
        .unwrap_err();
        assert!(matches!(err, ReminderError::TimeInPast(_)));
    }

    #[test]
    fn an_explicit_tomorrow_morning_resolves_exactly() {
        let (now_secs, now) = pinned();
        let due = resolve_due(
            &When::At {
                day_offset: Some(1),
                hour: 8,
                minute: 30,
            },
            now_secs,
            &now,
        )
        .unwrap();
        assert_eq!(due, (now_secs + 86_400 - 8 * 3600 + 30 * 60) as u64 * 1000);
        let spoken = spoken_when((due / 1000) as i64, &now);
        assert_eq!(spoken, "明天8点30分");
    }

    #[test]
    fn bad_shapes_are_named_errors() {
        let (now_secs, now) = pinned();
        assert!(resolve_due(&When::DelayMinutes(0), now_secs, &now).is_err());
        assert!(resolve_due(&When::DelayMinutes(100_081), now_secs, &now).is_err());
        assert!(resolve_due(
            &When::At {
                day_offset: None,
                hour: 24,
                minute: 0
            },
            now_secs,
            &now
        )
        .is_err());
        assert!(resolve_due(
            &When::At {
                day_offset: Some(366),
                hour: 8,
                minute: 0
            },
            now_secs,
            &now
        )
        .is_err());
    }

    fn svc() -> ReminderService {
        ReminderService::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn create_pending_cancel_is_the_whole_life_cycle() {
        let s = svc();
        let now = 1_000_000u64;
        let r = s.create("dot2-d", now + 600_000, "关火", now).unwrap();
        assert_eq!(r.state(), "pending");
        assert_eq!(s.pending(Some("dot2-d")).unwrap().len(), 1);
        assert_eq!(s.due(now + 599_999).unwrap().len(), 0);
        assert_eq!(s.due(now + 600_001).unwrap().len(), 1);

        let c = s.cancel(r.id, now + 1_000).unwrap();
        assert_eq!(c.state(), "cancelled");
        assert!(s.pending(Some("dot2-d")).unwrap().is_empty());
        assert!(matches!(
            s.cancel(r.id, now + 2_000).unwrap_err(),
            ReminderError::NotFound(_)
        ));
    }

    #[test]
    fn empty_and_oversized_notes_are_refused() {
        let s = svc();
        assert!(matches!(
            s.create("d", 2_000_000, "  ", 1_000_000).unwrap_err(),
            ReminderError::BadArgs(_)
        ));
        assert!(matches!(
            s.create("d", 2_000_000, &"长".repeat(41), 1_000_000)
                .unwrap_err(),
            ReminderError::BadArgs(_)
        ));
    }

    #[test]
    fn a_past_due_ms_is_refused_at_creation() {
        let s = svc();
        assert!(matches!(
            s.create("d", 999_999, "关火", 1_000_000).unwrap_err(),
            ReminderError::TimeInPast(_)
        ));
    }

    #[test]
    fn the_per_device_limit_is_enforced_with_a_named_error() {
        let s = svc();
        let now = 1_000_000u64;
        for i in 0..MAX_PENDING_PER_DEVICE {
            s.create(
                "d",
                now + 600_000 + i as u64 * 1_000,
                &format!("第{i}件"),
                now,
            )
            .unwrap();
        }
        assert!(matches!(
            s.create("d", now + 900_000, "超了", now).unwrap_err(),
            ReminderError::Limit(_)
        ));
        // Another device is unaffected (the limit is per speaker).
        s.create("other", now + 900_000, "另一台", now).unwrap();
    }

    #[test]
    fn mark_fired_is_at_most_once() {
        let s = svc();
        let now = 1_000_000u64;
        let r = s.create("d", now + 600_000, "关火", now).unwrap();
        assert!(s.mark_fired(r.id, now + 600_001).unwrap());
        assert!(
            !s.mark_fired(r.id, now + 600_002).unwrap(),
            "a second claim must not succeed"
        );
        assert_eq!(s.pending(Some("d")).unwrap().len(), 0);
        assert_eq!(s.recent(10).unwrap()[0].state(), "fired");
    }
}
