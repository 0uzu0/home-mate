//! Wake arbitration (2026-11-15): one phrase, five speakers, one answer.
//!
//! The problem is a consequence of the fleet becoming uniform: every speaker
//! in the house runs the same wake word and the same word list, so a person
//! saying 「你好小智」 in the living room wakes the bedroom one too. Before
//! this module the household answered N times (dot2-e answered the television
//! from another room, 2026-11-15).
//!
//! The rule is the user's (2026-11-14): **first come first served, plus an
//! acoustic fallback**. It is decided from SPATIAL evidence — when each node
//! heard the phrase and how loud it was — and never from the wake word, a
//! device name or a designated judge:
//!
//! - Every node that spots the phrase publishes one CLAIM: `heard_ms` (the
//!   wall clock at which the phrase's last audio reached it, `ts - lag_ms`)
//!   and `rms_dbfs` (the peak post-AEC level over the phrase).
//! - Claims are exchanged symmetrically: each node POSTs its claim to every
//!   peer (`/api/v0/fleet/wake`) and answers with its own, so one round trip
//!   gives both sides the same evidence. There is no arbiter process and no
//!   leader: [`decide`] is a pure function, so identical inputs give the
//!   identical winner on every node.
//! - The earliest claim wins. Claims within [`TIE_MS`] of the earliest are
//!   "simultaneous", and then the loudest wins if it is louder by at least
//!   [`LOUDER_DB`]; if the acoustics cannot separate them either, a fixed
//!   order decides and the fact is recorded as a tiebreak (never silently).
//!
//! Claims further apart than [`COALESCE_MS`] describe DIFFERENT utterances
//! (two people, two rooms, seconds apart) and never compete — each of those
//! should be answered in its own room.
//!
//! Degradation is explicit: a node with no peers, or whose peers do not
//! answer, behaves exactly as before this module existed (local answer, no
//! added latency) and records `arbitration_unavailable`. Nothing here can
//! delay a retreat/stop: those never reach this module (see `on_keyword`).

use crate::fleet::FleetService;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Two claims this close are the same utterance heard twice (2400 ms covers
/// the measured 1.3–1.4 s cross-room detection spread across Dot2 rooms).
pub const COALESCE_MS: i64 = 2400;
/// Max tracked peer clock entries and cold-start one-way skew clamp (#241).
pub const MAX_PEER_CLOCKS: usize = 64;
pub const MAX_COLD_CLOCK_OFFSET_MS: i64 = 60_000;
/// Claims this close were "heard at the same moment" — time cannot separate
/// them, so loudness gets the vote.
pub const TIE_MS: i64 = 150;
/// How much louder the later claim must be to overturn "first".
pub const LOUDER_DB: f32 = 3.0;
/// Absolute loudness floor (dBFS) required before loudness is treated as
/// sufficient spatial evidence. Below this floor, claims are too quiet to
/// reliably separate rooms and fall through to arrival-time comparison.
pub const MIN_LOUDNESS_EVIDENCE_DBFS: f32 = -50.0;
/// Minimum near-end SNR (dB) required for a claim's loudness to be trusted.
pub const MIN_SNR_DB: f32 = 6.0;
/// How long a node waits for peer claims before acting (200 ms, plus late
/// claims up to [`COALESCE_MS`] via [`Arbiter::late_claims`]).
pub const WINDOW_MS: u64 = 200;

/// One node's report of hearing the wake phrase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub device: String,
    /// `ts - lag_ms` on the reporting node: when the phrase's last audio
    /// reached that microphone. Wall clock, so comparable across nodes that
    /// are NTP-synced (all of them are: ntpd runs on every node).
    pub heard_ms: i64,
    /// Peak post-AEC level over the phrase. `None` = this node does not
    /// measure levels (an older firmware); such a claim can win on time but
    /// never on loudness, which is the honest reading.
    #[serde(default)]
    pub rms_dbfs: Option<f32>,
    /// Whether this node's `mic_gain_offset_db` has been field-calibrated.
    /// When any measured claim is uncalibrated, loudness wins are recorded as
    /// [`Why::LouderUncalibrated`] instead of [`Why::Louder`].
    #[serde(default)]
    pub calibrated: bool,
    /// Display-only room label for the winning device (`winner_room`).
    /// Never weights or orders the arbitration decision.
    #[serde(default)]
    pub room: String,
    /// Which word was spotted (they are all the wake word today, but the
    /// claim says what it was rather than assuming).
    #[serde(default)]
    pub word: String,
    /// Near-end SNR (dB) measured by the gateway, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f32>,
    /// True if the microphone signal clipped during the detection window.
    #[serde(default)]
    pub clipped: bool,
    /// Gateway echo-health state ("healthy", "degraded", ...) when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo_health: Option<String>,
    /// Acoustic profile version used by the gateway when computing levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_version: Option<String>,
    /// True when this device's room currently owns an active voice session
    /// (grants `SESSION_STICKY_BONUS_DB` continuity priority).
    #[serde(default)]
    pub session_sticky: bool,
    /// Bounded half-RTT clock alignment uncertainty (ms) after normalizing a
    /// peer's `heard_ms` into local wall-clock time (#76 / #171). `None` means
    /// no RTT clock-offset alignment was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_error_ms: Option<u64>,
    /// Sender wall-clock timestamp stamped by `FleetService::wake_claim` right
    /// before HTTP transmit so cold-start receivers can normalize `heard_ms`
    /// on the first packet without waiting for a prior RTT round (#76 / #171).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at_ms: Option<i64>,
}

impl Claim {
    pub fn new(device: impl Into<String>, heard_ms: i64, rms_dbfs: Option<f32>) -> Self {
        Self {
            device: device.into(),
            heard_ms,
            rms_dbfs,
            calibrated: false,
            room: String::new(),
            word: String::new(),
            snr_db: None,
            clipped: false,
            echo_health: None,
            profile_version: None,
            session_sticky: false,
            clock_error_ms: None,
            sent_at_ms: None,
        }
    }

    pub fn with_calibrated(mut self, calibrated: bool) -> Self {
        self.calibrated = calibrated;
        self
    }

    pub fn with_room(mut self, room: impl Into<String>) -> Self {
        self.room = room.into();
        self
    }

    pub fn with_word(mut self, word: impl Into<String>) -> Self {
        self.word = word.into();
        self
    }

    pub fn with_snr_db(mut self, snr_db: Option<f32>) -> Self {
        self.snr_db = snr_db;
        self
    }

    #[cfg(test)]
    pub fn with_clipped(mut self, clipped: bool) -> Self {
        self.clipped = clipped;
        self
    }

    #[cfg(test)]
    pub fn with_echo_health(mut self, echo_health: Option<String>) -> Self {
        self.echo_health = echo_health;
        self
    }

    #[cfg(test)]
    pub fn with_profile_version(mut self, profile_version: Option<String>) -> Self {
        self.profile_version = profile_version;
        self
    }

    pub fn with_session_sticky(mut self, session_sticky: bool) -> Self {
        self.session_sticky = session_sticky;
        self
    }

    pub fn with_clock_error_ms(mut self, clock_error_ms: Option<u64>) -> Self {
        self.clock_error_ms = clock_error_ms;
        self
    }
}

/// Why the winner won. Recorded, never inferred later from the margins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// Nobody was within [`TIE_MS`]: the earliest claim.
    First,
    /// Loudness decided the winner with all measured claims calibrated.
    Louder,
    /// Loudness decided the winner, but at least one measured claim is
    /// uncalibrated (`!calibrated`).
    LouderUncalibrated,
    /// Active room session continuity (`session_sticky`) decided the winner.
    SessionSticky,
    /// Peers lacked comparable acoustic level evidence (missing `rms_dbfs`,
    /// clipped/degraded/low-SNR exclusion, or `profile_version` mismatch),
    /// so arrival time decided.
    IncompleteFallback,
    /// Time and loudness both failed to separate the leaders: the fixed order
    /// decided.
    TieBreak,
    /// This node is the only claimant (no peers answered, or none of them
    /// heard the phrase).
    Solo,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub winner: Claim,
    pub why: Why,
    /// Winner vs the strongest OTHER claim: how much earlier (ms, positive =
    /// winner was earlier) and how much louder (dB, positive = winner was
    /// louder). `None` for a solo decision.
    pub margin_ms: Option<i64>,
    pub margin_db: Option<f32>,
    /// How many claims were considered (the coalesced phrase).
    pub considered: usize,
}

/// Decide around OUR OWN claim: the question a node asks is "who else heard
/// the phrase I just heard", so the anchor is the local detection, never the
/// earliest claim in the set.
///
/// This is a fix, not a refinement (dot2-b, 2026-11-15 23:38): a claim from an
/// EARLIER utterance in the same round became the leader, everything local was
/// then "too late" to be the same phrase, the local node lost to a phrase it
/// never heard, and the wrong room answered — three times in a row.
pub fn decide_around(local: &Claim, claims: &[Claim], preferred: &[String]) -> Option<Decision> {
    let cluster: Vec<Claim> = claims
        .iter()
        .filter(|c| (c.heard_ms - local.heard_ms).abs() <= COALESCE_MS)
        .cloned()
        .collect();
    if cluster.is_empty() {
        return decide_inner(std::slice::from_ref(local), preferred, false);
    }
    // Skip the redundant `first_ms` coalesce re-filtering inside `decide_inner`:
    // `cluster` is already coalesced around `local`, and re-filtering around
    // `min(heard_ms)` breaks symmetry for 3+ speakers at window edges.
    decide_inner(&cluster, preferred, false)
}

/// The whole policy. Pure: same claims + same order ⇒ same winner, on every
/// node in the house.
///
/// `preferred` is the fixed fallback order (device ids). It exists so a
/// genuine tie is decided by a rule every node can compute rather than by
/// clock jitter; an empty list falls back to lexicographic device id, which
/// is just as fixed.
pub fn decide(claims: &[Claim], preferred: &[String]) -> Option<Decision> {
    decide_inner(claims, preferred, true)
}

fn decide_inner(claims: &[Claim], preferred: &[String], coalesce: bool) -> Option<Decision> {
    if claims.is_empty() {
        return None;
    }
    // 1. Coalesce: only the claims that belong to the SAME utterance compete.
    let mut group: Vec<&Claim> = if coalesce {
        let first_ms = claims.iter().map(|c| c.heard_ms).min().unwrap();
        claims
            .iter()
            .filter(|c| (c.heard_ms - first_ms).abs() <= COALESCE_MS)
            .collect()
    } else {
        claims.iter().collect()
    };
    if group.is_empty() {
        return None;
    }
    group.sort_by(|a, b| {
        a.heard_ms
            .cmp(&b.heard_ms)
            .then_with(|| a.device.cmp(&b.device))
    });

    if group.len() == 1 {
        return Some(Decision {
            winner: group[0].clone(),
            why: Why::Solo,
            margin_ms: None,
            margin_db: None,
            considered: 1,
        });
    }

    // 2. LOUDNESS FIRST (user decision 2026-11-15, after measuring). The same
    //    phrase reaches every microphone at once, but each model fires when
    //    ITS confidence crosses the bar — measured 1.3–1.4 s of spread between
    //    rooms. Arrival time is therefore noise for ranking rooms, while the
    //    level is the spatial fact ("who is in the room with the person").
    //
    //    The louder claim wins when it is louder by LOUDER_DB; when the
    //    acoustics cannot separate them, arrival time decides; when even that
    //    is identical, a fixed order does and the record says `tie_break`.
    //    A claim without a level never wins on loudness (it cannot prove one).
    let margin_of = |w: &Claim| -> (Option<i64>, Option<f32>) {
        let other = group
            .iter()
            .copied()
            .filter(|c| c.device != w.device)
            .min_by_key(|c| c.heard_ms);
        match other {
            Some(r) => (
                Some(r.heard_ms - w.heard_ms),
                match (w.rms_dbfs, r.rms_dbfs) {
                    (Some(a), Some(b)) => Some(a - b),
                    _ => None,
                },
            ),
            None => (None, None),
        }
    };

    let distinct_profiles: std::collections::BTreeSet<&str> = group
        .iter()
        .filter_map(|c| {
            c.profile_version
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect();
    let profile_mismatch = distinct_profiles.len() > 1
        || (!distinct_profiles.is_empty()
            && group.iter().any(|c| {
                c.profile_version
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(str::is_empty)
            }));

    let loudness_eligible = |c: &Claim| -> Option<f32> {
        if c.clipped {
            return None;
        }
        if let Some(ref health) = c.echo_health {
            let trimmed = health.trim();
            if !matches!(trimmed, "" | "healthy" | "unknown" | "idle_unlocked") {
                return None;
            }
        }
        if let Some(snr) = c.snr_db {
            if !snr.is_finite() || snr < MIN_SNR_DB {
                return None;
            }
        }
        c.rms_dbfs.filter(|db| db.is_finite() && *db >= -200.0)
    };

    let mut levels: Vec<(&Claim, f32, f32)> = if profile_mismatch {
        Vec::new()
    } else {
        group
            .iter()
            .copied()
            .filter_map(|c| {
                loudness_eligible(c).map(|db| {
                    let bonus = if c.session_sticky {
                        crate::sovereignty::SESSION_STICKY_BONUS_DB
                    } else {
                        0.0
                    };
                    (c, db, db + bonus)
                })
            })
            .collect()
    };

    let max_active_raw = group
        .iter()
        .filter(|c| !c.word.trim().is_empty())
        .filter_map(|c| c.rms_dbfs.filter(|db| db.is_finite()))
        .fold(None::<f32>, |acc, db| Some(acc.map_or(db, |a| a.max(db))));
    if levels.len() >= 2 {
        levels.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let (loudest, loudest_raw, loudest_eff) = levels[0];
        let (_, second_raw, second_eff) = levels[1];
        // Require the loudest claim to clear MIN_LOUDNESS_EVIDENCE_DBFS and
        // beat the runner-up by at least LOUDER_DB. A passive presence claim
        // (`word == ""`) may only win on loudness if it also beats every active
        // wake claim (`word != ""`) in the group by at least LOUDER_DB.
        let passive_blocked = loudest.word.trim().is_empty()
            && max_active_raw.is_some_and(|act| loudest_raw < act + LOUDER_DB);
        if !passive_blocked
            && loudest_raw >= MIN_LOUDNESS_EVIDENCE_DBFS
            && loudest_eff - second_eff >= LOUDER_DB
        {
            let best_other_raw = levels[1..]
                .iter()
                .map(|(_, raw, _)| *raw)
                .fold(f32::NEG_INFINITY, f32::max);
            let sticky_decided =
                loudest.session_sticky && (loudest_raw - best_other_raw) < LOUDER_DB;
            let why = if sticky_decided {
                Why::SessionSticky
            } else if levels.iter().any(|(c, _, _)| !c.calibrated) {
                Why::LouderUncalibrated
            } else {
                Why::Louder
            };
            let _ = second_raw;
            let (m_ms, m_db) = margin_of(loudest);
            return Some(Decision {
                winner: loudest.clone(),
                why,
                margin_ms: m_ms,
                margin_db: m_db,
                considered: group.len(),
            });
        }
    }

    let mut incomplete_evidence = profile_mismatch || levels.len() < 2;

    // 3. The loudest two measured claims are within LOUDER_DB (or below
    //    MIN_LOUDNESS_EVIDENCE_DBFS), or peers lack comparable level evidence.
    //    When all peers have comparable level evidence, arrival time only
    //    orders the claims that are still plausible rooms (>= MIN_LOUDNESS_EVIDENCE_DBFS
    //    and within LOUDER_DB of the loudest). When acoustic evidence is
    //    incomplete, everyone in the group competes on arrival time.
    let mut tier: Vec<&Claim> = if incomplete_evidence {
        group.clone()
    } else {
        match levels
            .iter()
            .filter(|(_, raw, _)| *raw >= MIN_LOUDNESS_EVIDENCE_DBFS)
            .map(|(_, _, eff)| *eff)
            .fold(None::<f32>, |best, db| match best {
                Some(b) if b >= db => best,
                _ => Some(db),
            }) {
            Some(top) => group
                .iter()
                .copied()
                .filter(|c| {
                    loudness_eligible(c).is_some_and(|raw| {
                        let eff = raw
                            + if c.session_sticky {
                                crate::sovereignty::SESSION_STICKY_BONUS_DB
                            } else {
                                0.0
                            };
                        raw >= MIN_LOUDNESS_EVIDENCE_DBFS && top - eff < LOUDER_DB
                    })
                })
                .collect(),
            None => group.clone(),
        }
    };
    let mut sticky_tier_narrowed = false;
    let sticky_only: Vec<&Claim> = tier.iter().copied().filter(|c| c.session_sticky).collect();
    if !sticky_only.is_empty() && sticky_only.len() < tier.len() {
        tier = sticky_only;
        sticky_tier_narrowed = true;
    }
    let max_tier_raw = tier
        .iter()
        .filter_map(|c| loudness_eligible(c))
        .fold(f32::NEG_INFINITY, f32::max);
    let kws_only: Vec<&Claim> = tier
        .iter()
        .copied()
        .filter(|c| !c.word.trim().is_empty())
        .collect();
    if !kws_only.is_empty() {
        tier = kws_only;
    } else if !sticky_tier_narrowed {
        let rescued: Vec<&Claim> = group
            .iter()
            .copied()
            .filter(|c| {
                !c.word.trim().is_empty()
                    && c.rms_dbfs
                        .filter(|v| v.is_finite())
                        .is_none_or(|act| max_tier_raw < act + LOUDER_DB)
            })
            .collect();
        if !rescued.is_empty() {
            tier = rescued;
            incomplete_evidence = true;
        }
    }
    let leader = pick_earliest(&tier, preferred);
    let exact: Vec<&Claim> = tier
        .iter()
        .copied()
        .filter(|c| {
            // Use `max` (not `sum`) so every observer node in a 3+ speaker
            // household evaluates the exact same uncertainty window regardless
            // of which peer is local (`error=0`) vs remote (`error=E`).
            let uncertainty = leader
                .clock_error_ms
                .unwrap_or(0)
                .max(c.clock_error_ms.unwrap_or(0)) as i64;
            (c.heard_ms - leader.heard_ms).abs() <= uncertainty
        })
        .collect();
    let (winner, why) = if exact.len() > 1 {
        (pick_fixed_order(&exact, preferred).clone(), Why::TieBreak)
    } else if sticky_tier_narrowed {
        (leader.clone(), Why::SessionSticky)
    } else if incomplete_evidence {
        (leader.clone(), Why::IncompleteFallback)
    } else {
        (
            leader.clone(),
            if group.len() == 1 {
                Why::Solo
            } else {
                Why::First
            },
        )
    };
    let (m_ms, m_db) = margin_of(&winner);
    Some(Decision {
        winner,
        why,
        margin_ms: m_ms,
        margin_db: m_db,
        considered: group.len(),
    })
}

/// Earliest claim; equal arrivals go to the fixed order so the answer never
/// depends on array order.
fn pick_earliest<'a>(group: &[&'a Claim], preferred: &[String]) -> &'a Claim {
    let first_ms = group[0].heard_ms;
    let same: Vec<&Claim> = group
        .iter()
        .copied()
        .filter(|c| c.heard_ms == first_ms)
        .collect();
    pick_fixed_order(&same, preferred)
}

/// The fixed fallback: `preferred` order when it names the devices, then
/// lexicographic id so the answer never depends on hash or arrival order.
fn pick_fixed_order<'a>(tied: &[&'a Claim], preferred: &[String]) -> &'a Claim {
    let rank = |c: &&Claim| -> usize {
        preferred
            .iter()
            .position(|p| p == &c.device)
            .unwrap_or(usize::MAX)
    };
    let mut best = tied[0];
    for c in tied.iter().skip(1) {
        let (rb, rc) = (rank(&best), rank(c));
        if rc < rb || (rc == rb && c.device < best.device) {
            best = c;
        }
    }
    best
}

// --- the runtime half ------------------------------------------------------
//
// Everything above is policy. The code below exchanges claims over the
// existing fleet transport (peer list + sealed bearer tokens, M12a-1) and
// holds the ONE open round per node. Deliberately small: a round is a few
// hundred milliseconds long, and a node only ever has one wake in flight.

/// Sliding window for retaining recent incoming peer claims even when no local
/// KWS round is open (`COALESCE_MS * 2` = 4.8 s).
const RECENT_FOREIGN_TTL_MS: u64 = (COALESCE_MS * 2) as u64;
/// How long an RTT-derived peer wall-clock offset estimate remains valid.
const PEER_CLOCK_TTL: Duration = Duration::from_secs(300);
/// Minimum scheduling margin added to half-RTT when bounding peer clock error.
const PEER_CLOCK_MARGIN_MS: u64 = 20;
/// Conservative one-way clock error bound used when normalizing a cold-start
/// inbound claim from its `sent_at_ms` header before a prior RTT sample exists.
const COLD_ONE_WAY_ERROR_MS: u64 = 40;

#[derive(Debug, Clone, Copy)]
struct PeerClockEstimate {
    /// Estimated `peer_wall_ms - local_wall_ms`.
    offset_ms: i64,
    /// Bounded uncertainty (`rtt_ms / 2 + PEER_CLOCK_MARGIN_MS`).
    error_ms: u64,
    measured_at: Instant,
}

#[derive(Clone)]
struct ForeignEntry {
    /// Original wire `heard_ms` in the peer's wall clock (so re-normalizing
    /// when a tighter RTT estimate arrives never double-subtracts `offset_ms`).
    raw_heard_ms: i64,
    claim: Claim,
    seen: Instant,
}

pub struct Arbiter {
    fleet: Arc<FleetService>,
    state: std::sync::Mutex<ArbiterState>,
}

#[derive(Default)]
struct ArbiterState {
    round: Option<Round>,
    recent_foreign: Vec<ForeignEntry>,
    /// RTT-bounded pairwise wall-clock offsets (`peer_id` / `device` -> estimate)
    /// used to normalize peer `heard_ms` into local clock time (#76 / #171).
    peer_clocks: std::collections::HashMap<String, PeerClockEstimate>,
    /// Fresh event-driven `NearEndPresence` claim for passive peer contention
    /// when this node did not fire local KWS.
    passive_presence: Option<(Claim, Instant)>,
    /// Last device that won arbitration (explicit or passive) for passive ASR
    /// resolution when KWS missed.
    last_won_at: Option<(String, Instant)>,
}

struct Round {
    id: String,
    claim: Claim,
    started: Instant,
    foreign: Vec<Claim>,
    exchanging: bool,
}

impl Arbiter {
    /// `fleet` is built from the same store the node already uses, so peers
    /// and tokens are the same objects the console and the config push see.
    pub fn new(store: Arc<crate::store::Store>) -> Arc<Self> {
        Arc::new(Self {
            fleet: FleetService::new(store),
            state: std::sync::Mutex::new(ArbiterState::default()),
        })
    }

    pub fn peer_count(&self) -> usize {
        self.fleet.doc().nodes.len()
    }

    fn claim_id(claim: &Claim) -> String {
        format!("{}:{}", claim.device, claim.heard_ms)
    }

    fn prune_recent_locked(recent: &mut Vec<ForeignEntry>) {
        let ttl = Duration::from_millis(RECENT_FOREIGN_TTL_MS);
        recent.retain(|e| e.seen.elapsed() <= ttl);
    }

    fn should_replace_foreign(existing: &Claim, incoming: &Claim) -> bool {
        let ex_active = !existing.word.trim().is_empty();
        let in_active = !incoming.word.trim().is_empty();
        if ex_active != in_active {
            return in_active;
        }
        let ex_rms = existing
            .rms_dbfs
            .filter(|v| v.is_finite())
            .unwrap_or(f32::NEG_INFINITY);
        let in_rms = incoming
            .rms_dbfs
            .filter(|v| v.is_finite())
            .unwrap_or(f32::NEG_INFINITY);
        if (in_rms - ex_rms).abs() > f32::EPSILON {
            return in_rms > ex_rms;
        }
        existing.clock_error_ms.is_none()
            || incoming
                .clock_error_ms
                .is_some_and(|e| existing.clock_error_ms.is_none_or(|old| e < old))
    }

    fn merge_foreign_claim(foreign: &mut Vec<Claim>, incoming: Claim) {
        if let Some(existing) = foreign.iter_mut().find(|c| c.device == incoming.device) {
            if Self::should_replace_foreign(existing, &incoming) {
                *existing = incoming;
            }
        } else {
            foreign.push(incoming);
        }
    }

    fn record_recent_locked(recent: &mut Vec<ForeignEntry>, raw_heard_ms: i64, claim: Claim) {
        Self::prune_recent_locked(recent);
        if let Some(existing) = recent.iter_mut().find(|e| e.claim.device == claim.device) {
            if (claim.heard_ms - existing.claim.heard_ms).abs() > COALESCE_MS
                || Self::should_replace_foreign(&existing.claim, &claim)
            {
                *existing = ForeignEntry {
                    raw_heard_ms,
                    claim,
                    seen: Instant::now(),
                };
            }
            return;
        }
        recent.push(ForeignEntry {
            raw_heard_ms,
            claim,
            seen: Instant::now(),
        });
    }

    fn record_peer_clock_locked(
        peer_clocks: &mut std::collections::HashMap<String, PeerClockEstimate>,
        peer_key: &str,
        server_time_ms: i64,
        local_midpoint_ms: i64,
        rtt_ms: u64,
    ) {
        let key = peer_key.trim();
        if key.is_empty() {
            return;
        }
        peer_clocks.retain(|_, e| e.measured_at.elapsed() <= PEER_CLOCK_TTL);
        while peer_clocks.len() >= MAX_PEER_CLOCKS && !peer_clocks.contains_key(key) {
            let Some(oldest) = peer_clocks
                .iter()
                .min_by_key(|(_, e)| e.measured_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            peer_clocks.remove(&oldest);
        }
        let est = PeerClockEstimate {
            offset_ms: server_time_ms.saturating_sub(local_midpoint_ms),
            error_ms: (rtt_ms / 2).saturating_add(PEER_CLOCK_MARGIN_MS),
            measured_at: Instant::now(),
        };
        peer_clocks.insert(key.to_string(), est);
    }

    #[cfg(test)]
    pub(crate) fn record_peer_clock_for_test(&self, key: &str, srv: i64, mid: i64, rtt: u64) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        Self::record_peer_clock_locked(&mut st.peer_clocks, key, srv, mid, rtt);
    }

    #[cfg(test)]
    pub(crate) fn peer_clock_count_for_test(&self) -> usize {
        crate::lock_or_recover(&self.state, "arbitration.state")
            .peer_clocks
            .len()
    }

    fn normalize_claim_locked(
        peer_clocks: &std::collections::HashMap<String, PeerClockEstimate>,
        raw_heard_ms: i64,
        claim: &mut Claim,
        peer_id_hint: Option<&str>,
    ) {
        claim.clock_error_ms = None;
        claim.sent_at_ms = None;
        let est = peer_clocks
            .get(&claim.device)
            .or_else(|| peer_id_hint.and_then(|id| peer_clocks.get(id)))
            .filter(|e| e.measured_at.elapsed() <= PEER_CLOCK_TTL);
        if let Some(e) = est {
            claim.heard_ms = raw_heard_ms.saturating_sub(e.offset_ms);
            claim.clock_error_ms = Some(e.error_ms);
        } else {
            claim.heard_ms = raw_heard_ms;
        }
    }

    /// Record an RTT-bounded peer wall-clock sample (`server_time_ms - local_midpoint_ms`).
    pub fn record_peer_clock(
        &self,
        peer_key: &str,
        server_time_ms: i64,
        local_midpoint_ms: i64,
        rtt_ms: u64,
    ) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        Self::record_peer_clock_locked(
            &mut st.peer_clocks,
            peer_key,
            server_time_ms,
            local_midpoint_ms,
            rtt_ms,
        );
    }

    /// Return the fresh `(offset_ms, error_ms)` estimate for `peer_key`, if any.
    pub fn peer_clock_estimate(&self, peer_key: &str) -> Option<(i64, u64)> {
        let st = crate::lock_or_recover(&self.state, "arbitration.state");
        st.peer_clocks
            .get(peer_key)
            .filter(|e| e.measured_at.elapsed() <= PEER_CLOCK_TTL)
            .map(|e| (e.offset_ms, e.error_ms))
    }

    /// Open a round for this node's claim. Returns the round id.
    ///
    /// A previous round older than [`COALESCE_MS`] is discarded: it belongs to
    /// a different utterance and must not contaminate this one. Re-triggers on
    /// the same device within [`COALESCE_MS`] update the round id and claim
    /// consistently while preserving compatible peer claims.
    pub fn begin(&self, claim: Claim) -> String {
        let id = Self::claim_id(&claim);
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        Self::prune_recent_locked(&mut st.recent_foreign);
        let seeded: Vec<Claim> = st
            .recent_foreign
            .iter()
            .filter(|e| {
                e.claim.device != claim.device
                    && (e.claim.heard_ms - claim.heard_ms).abs() <= COALESCE_MS
            })
            .map(|e| e.claim.clone())
            .collect();
        let coalesce = Duration::from_millis(COALESCE_MS as u64);
        match st.round.as_mut() {
            Some(r) if r.id == id => {
                r.exchanging = true;
            }
            Some(r) if r.started.elapsed() <= coalesce && r.claim.device == claim.device => {
                let mut foreign: Vec<Claim> = r
                    .foreign
                    .iter()
                    .filter(|c| (c.heard_ms - claim.heard_ms).abs() <= COALESCE_MS)
                    .cloned()
                    .collect();
                for s in seeded {
                    Self::merge_foreign_claim(&mut foreign, s);
                }
                *r = Round {
                    id: id.clone(),
                    claim,
                    started: Instant::now(),
                    foreign,
                    exchanging: true,
                };
            }
            _ => {
                st.round = Some(Round {
                    id: id.clone(),
                    claim,
                    started: Instant::now(),
                    foreign: seeded,
                    exchanging: true,
                });
            }
        }
        id
    }

    /// Record a fresh event-driven `NearEndPresence` claim so this node can
    /// enter peer contention passively when queried by a peer even without KWS.
    /// Within the same `COALESCE_MS` utterance window, preserves the utterance's
    /// vocal peak `rms_dbfs` and `snr_db` so a softer trailing syllable never
    /// downgrades the wake peak.
    pub fn record_passive_presence(&self, mut claim: Claim) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        if let Some((prev, seen)) = st.passive_presence.as_ref() {
            if prev.device == claim.device
                && seen.elapsed() <= Duration::from_millis(COALESCE_MS as u64)
                && (claim.heard_ms - prev.heard_ms).abs() <= COALESCE_MS
            {
                if let (Some(p_rms), Some(c_rms)) = (prev.rms_dbfs, claim.rms_dbfs) {
                    if p_rms > c_rms {
                        claim.rms_dbfs = Some(p_rms);
                        claim.snr_db = match (prev.snr_db, claim.snr_db) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            (a, b) => a.or(b),
                        };
                    }
                }
            }
        }
        st.passive_presence = Some((claim, Instant::now()));
    }

    /// Refresh the liveness timestamp of an existing `passive_presence` for
    /// `device` when its speech segment closes and enters asynchronous ASR
    /// recognition, so a multi-second utterance + ASR round-trip does not
    /// expire right before `admit_asr_wake` runs.
    pub fn refresh_passive_presence(&self, device: &str) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        if let Some((claim, seen)) = st.passive_presence.as_mut() {
            if claim.device == device
                && seen.elapsed() <= Duration::from_millis((COALESCE_MS as u64) * 2)
            {
                *seen = Instant::now();
            }
        }
    }

    /// Return this node's fresh passive `NearEndPresence` claim for `device`
    /// if it was recorded within `COALESCE_MS`.
    pub fn passive_presence_for(&self, device: &str) -> Option<Claim> {
        let st = crate::lock_or_recover(&self.state, "arbitration.state");
        st.passive_presence
            .as_ref()
            .filter(|(c, seen)| {
                c.device == device && seen.elapsed() <= Duration::from_millis(COALESCE_MS as u64)
            })
            .map(|(c, _)| c.clone())
    }

    pub fn clear_passive_presence(&self, device: &str) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        if st
            .passive_presence
            .as_ref()
            .is_some_and(|(c, _)| c.device == device)
        {
            st.passive_presence = None;
        }
    }

    /// Record that `device` won arbitration (so passive ASR resolution can
    /// transcribe its segment even if `proactive_asr_wake` is off).
    pub fn record_win(&self, device: &str) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        st.last_won_at = Some((device.to_string(), Instant::now()));
    }

    pub fn won_recently(&self, device: &str, within: Duration) -> bool {
        let st = crate::lock_or_recover(&self.state, "arbitration.state");
        st.last_won_at
            .as_ref()
            .is_some_and(|(d, at)| d == device && at.elapsed() <= within)
    }

    /// Synchronously revoke `last_won_at` when `device` loses arbitration
    /// (`#116` / `ARB-09`), so stale recent-win eligibility cannot admit
    /// subsequent passive ASR segments.
    pub fn clear_win(&self, device: &str) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        if st.last_won_at.as_ref().is_some_and(|(d, _)| d == device) {
            st.last_won_at = None;
        }
    }

    /// A peer's claim arrived. Always retains it in the sliding window
    /// (`COALESCE_MS * 2`), records it in the open round when it belongs to the
    /// open phrase, and returns this node's own claim (or fresh passive
    /// `NearEndPresence` claim) so the peer learns it from the same round trip.
    pub fn inbound(&self, mut claim: Claim) -> Value {
        let server_time_ms = crate::id::now_ms();
        let raw_heard_ms = claim.heard_ms;
        let sent_at_ms = claim.sent_at_ms.take();
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        Self::normalize_claim_locked(&st.peer_clocks, raw_heard_ms, &mut claim, None);
        if claim.clock_error_ms.is_none() {
            if let Some(sent_ms) = sent_at_ms {
                let cold_offset = sent_ms
                    .saturating_sub(server_time_ms as i64)
                    .clamp(-MAX_COLD_CLOCK_OFFSET_MS, MAX_COLD_CLOCK_OFFSET_MS);
                claim.heard_ms = raw_heard_ms.saturating_sub(cold_offset);
                claim.clock_error_ms = Some(COLD_ONE_WAY_ERROR_MS);
            }
        }
        Self::record_recent_locked(&mut st.recent_foreign, raw_heard_ms, claim.clone());
        let Some(round) = st.round.as_mut() else {
            if let Some((passive, seen)) = st.passive_presence.as_ref().cloned() {
                if seen.elapsed() <= Duration::from_millis(COALESCE_MS as u64)
                    && (passive.heard_ms - claim.heard_ms).abs() <= COALESCE_MS
                {
                    let mut all = vec![passive.clone()];
                    for entry in &st.recent_foreign {
                        if entry.claim.device != passive.device
                            && (entry.claim.heard_ms - passive.heard_ms).abs() <= COALESCE_MS
                        {
                            Self::merge_foreign_claim(&mut all, entry.claim.clone());
                        }
                    }
                    if let Some(dec) = decide_around(&passive, &all, &[]) {
                        if dec.winner.device == passive.device {
                            st.last_won_at = Some((passive.device.clone(), Instant::now()));
                        } else if st
                            .last_won_at
                            .as_ref()
                            .is_some_and(|(d, _)| d == &passive.device)
                        {
                            st.last_won_at = None;
                        }
                    }
                    return json!({
                        "state": "passive_presence",
                        "claim": passive,
                        "server_time_ms": server_time_ms,
                    });
                }
            }
            // No wake in flight and no fresh passive presence: answer with
            // "nothing of mine to compare" so the peer's round does not wait.
            return json!({
                "state": "no_round",
                "claim": Value::Null,
                "server_time_ms": server_time_ms,
            });
        };
        // Only claims about THIS utterance are collected — and for a
        // different one we hand back NOTHING. Returning our own claim there
        // was a live defect (dot2-b, 2026-11-15 23:38): the peer merged a
        // claim from four seconds earlier into its round and lost to it.
        if (claim.heard_ms - round.claim.heard_ms).abs() > COALESCE_MS {
            return json!({
                "state": "other_utterance",
                "claim": Value::Null,
                "server_time_ms": server_time_ms,
            });
        }
        Self::merge_foreign_claim(&mut round.foreign, claim);
        json!({
            "state": "collected",
            "claim": round.claim,
            "server_time_ms": server_time_ms,
        })
    }

    /// Whether an initial `WINDOW_MS` exchange is currently in-flight for `device`.
    pub fn is_exchanging(&self, device: &str) -> bool {
        let st = crate::lock_or_recover(&self.state, "arbitration.state");
        st.round.as_ref().is_some_and(|r| {
            r.claim.device == device
                && r.exchanging
                && r.started.elapsed() <= Duration::from_millis(WINDOW_MS + 50)
        })
    }

    /// Recent incoming peer claims within the `COALESCE_MS * 2` sliding window.
    pub fn recent_foreign_claims(&self, exclude_device: &str) -> Vec<Claim> {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        Self::prune_recent_locked(&mut st.recent_foreign);
        st.recent_foreign
            .iter()
            .filter(|e| e.claim.device != exclude_device)
            .map(|e| e.claim.clone())
            .collect()
    }

    /// Publish our claim to every peer and wait `window` for theirs.
    ///
    /// Returns every claim of this utterance known to this node (ours first),
    /// plus how many peers were reachable. The caller decides; this never
    /// blocks anything but its own await.
    pub async fn exchange(&self, id: &str, window: Duration) -> (Vec<Claim>, usize, usize) {
        let claim = {
            let st = crate::lock_or_recover(&self.state, "arbitration.state");
            match st.round.as_ref().filter(|r| r.id == id) {
                Some(r) => r.claim.clone(),
                None => return (Vec::new(), 0, self.peer_count()),
            }
        };
        let peers = self.peer_count();
        let body = serde_json::to_value(&claim).unwrap_or(Value::Null);
        let deadline = Instant::now() + window;
        // One budget for the whole exchange, not one per peer: the requests
        // run concurrently and each is capped at half the window, so the wait
        // below is the remainder of a bound that cannot grow with the fleet.
        let per_request = window.min(Duration::from_millis(80));
        let (answers, replies) = self.fleet.wake_claim(&body, per_request).await;
        // Peers answer with their own claim; inbound POSTs from peers that
        // broadcast first arrive through `inbound`. Both are the same
        // evidence, so they merge into one set (deduplicated by device).
        {
            let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
            let mut parsed_claims: Vec<(i64, Claim)> = Vec::new();
            for reply in replies {
                if let Some(server_time_ms) = reply.server_time_ms {
                    Self::record_peer_clock_locked(
                        &mut st.peer_clocks,
                        &reply.peer_id,
                        server_time_ms,
                        reply.local_midpoint_ms,
                        reply.rtt_ms,
                    );
                }
                if let Some(v) = reply.claim {
                    if let Ok(mut c) = serde_json::from_value::<Claim>(v) {
                        let raw_heard_ms = c.heard_ms;
                        if let Some(server_time_ms) = reply.server_time_ms {
                            Self::record_peer_clock_locked(
                                &mut st.peer_clocks,
                                &c.device,
                                server_time_ms,
                                reply.local_midpoint_ms,
                                reply.rtt_ms,
                            );
                        }
                        Self::normalize_claim_locked(
                            &st.peer_clocks,
                            raw_heard_ms,
                            &mut c,
                            Some(&reply.peer_id),
                        );
                        parsed_claims.push((raw_heard_ms, c));
                    }
                }
            }
            for (raw_heard_ms, c) in parsed_claims {
                Self::record_recent_locked(&mut st.recent_foreign, raw_heard_ms, c);
            }
            // Retroactively re-normalize buffered foreign claims from `raw_heard_ms`
            // whenever a fresher RTT estimate is now available, then promote any
            // compatible claims into the active round.
            Self::prune_recent_locked(&mut st.recent_foreign);
            let peer_clocks_snapshot = st.peer_clocks.clone();
            for entry in &mut st.recent_foreign {
                if peer_clocks_snapshot
                    .get(&entry.claim.device)
                    .is_some_and(|e| e.measured_at.elapsed() <= PEER_CLOCK_TTL)
                {
                    Self::normalize_claim_locked(
                        &peer_clocks_snapshot,
                        entry.raw_heard_ms,
                        &mut entry.claim,
                        None,
                    );
                }
            }
            let recent_claims: Vec<Claim> =
                st.recent_foreign.iter().map(|e| e.claim.clone()).collect();
            if let Some(r) = st.round.as_mut().filter(|r| r.id == id) {
                for c in recent_claims {
                    if c.device == r.claim.device
                        || (c.heard_ms - r.claim.heard_ms).abs() > COALESCE_MS
                    {
                        continue;
                    }
                    Self::merge_foreign_claim(&mut r.foreign, c);
                }
            }
        }
        // Give the reverse broadcast the rest of the window to land.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            tokio::time::sleep(remaining).await;
        }
        let (claims, answered) = {
            let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
            Self::prune_recent_locked(&mut st.recent_foreign);
            let recent_claims: Vec<Claim> =
                st.recent_foreign.iter().map(|e| e.claim.clone()).collect();
            let mut all = Vec::new();
            if let Some(r) = st.round.as_mut().filter(|r| r.id == id) {
                for c in recent_claims {
                    if c.device != r.claim.device
                        && (c.heard_ms - r.claim.heard_ms).abs() <= COALESCE_MS
                    {
                        Self::merge_foreign_claim(&mut r.foreign, c);
                    }
                }
                r.exchanging = false;
                all.push(r.claim.clone());
                for c in &r.foreign {
                    if !all.iter().any(|a| a.device == c.device) {
                        all.push(c.clone());
                    }
                }
            }
            (all, answers)
        };
        // ARB-03: when multiple foreign peers returned claims (e.g. two passive
        // nodes both stronger than this active node) and a foreign peer won,
        // propagate the winning peer's claim once so runner-up passive peers
        // record it in `recent_foreign` and revoke their provisional `last_won_at`.
        if claims.len() > 2 {
            if let Some(dec) = decide_around(&claim, &claims, &[]) {
                if dec.winner.device != claim.device {
                    if let Ok(winner_body) = serde_json::to_value(&dec.winner) {
                        let _ = self.fleet.wake_claim(&winner_body, per_request).await;
                    }
                }
            }
        }
        (claims, answered, peers)
    }

    /// The claims that have arrived since the decision, for the late check.
    /// Does not close the round.
    pub fn claims_so_far(&self, id: &str) -> Vec<Claim> {
        let st = crate::lock_or_recover(&self.state, "arbitration.state");
        match st.round.as_ref().filter(|r| r.id == id) {
            Some(r) => {
                let mut all = vec![r.claim.clone()];
                for c in &r.foreign {
                    if !all.iter().any(|a| a.device == c.device) {
                        all.push(c.clone());
                    }
                }
                all
            }
            None => Vec::new(),
        }
    }

    /// Close the round. Idempotent; a late `inbound` after this simply finds
    /// no round and answers honestly.
    pub fn finish(&self, id: &str) {
        let mut st = crate::lock_or_recover(&self.state, "arbitration.state");
        if st.round.as_ref().map(|r| r.id.as_str()) == Some(id) {
            st.round = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(device: &str, heard: i64, rms: Option<f32>) -> Claim {
        Claim::new(device, heard, rms).with_calibrated(true)
    }

    /// The rule the user chose (2026-11-15): within one utterance the LOUDER
    /// room wins, even when its model reported it later — arrival time is
    /// dominated by per-device detection latency (measured 1.3–1.4 s spread),
    /// level is not.
    #[test]
    fn the_louder_claim_wins_even_when_it_arrived_later() {
        let claims = [
            c("dot2-e", 1000, Some(-30.0)),
            c("dot2-d", 1300, Some(-10.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(
            d.winner.device, "dot2-d",
            "20 dB louder beats 300 ms earlier"
        );
        assert_eq!(d.why, Why::Louder);
        assert_eq!(
            d.margin_ms,
            Some(-300),
            "negative = the winner arrived later"
        );
        assert!(d.margin_db.unwrap() >= 19.0);
    }

    /// The measured case that decided the rule (dot2 2026-11-15 22:06):
    /// 儿童房 d −25.3 dBFS at +1.40 s, 书房 b −28.6 at +1.31 s, 主卧 c −32.2
    /// at +0.0 s. The person was next to d; time-first picked b, the room next
    /// door. Loudness-first picks d.
    #[test]
    fn the_loudest_room_wins_on_the_measured_2026_11_15_case() {
        let claims = [
            c("dot2-c", 0, Some(-32.2)),
            c("dot2-b", 1310, Some(-28.6)),
            c("dot2-d", 1400, Some(-25.3)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-d");
        assert_eq!(d.why, Why::Louder);
        assert_eq!(d.considered, 3);
    }

    /// What is NOT overturned by loudness: two rooms 1.5 dB apart (the real
    /// 23:38:51 pair, −18.6 vs −17.1). Within LOUDER_DB the acoustics did not
    /// separate them, so arrival time keeps it.
    #[test]
    fn a_level_difference_within_the_margin_does_not_overturn_time() {
        let claims = [
            c("dot2-b", 1000, Some(-18.6)),
            c("dot2-d", 1065, Some(-17.1)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-b");
        assert_eq!(d.why, Why::First);
    }

    /// LIVE 2026-11-16 (desk test): dot2-e (客厅, −39.5 dBFS) was the EARLIEST
    /// claim by 91 ms and took the whole utterance, although dot2-d
    /// (儿童房, −30.5) and dot2-b (书房, −32.8) were 9 and 6.7 dB louder. The
    /// loudest two were only 2.3 dB apart, so the level could not separate
    /// them and the old fallback ("earliest") handed the answer to a room that
    /// was demonstrably NOT where the person spoke. Time is noise here
    /// (measured 1.3–1.4 s of spread); it may only order the claims that are
    /// still plausible rooms.
    #[test]
    fn the_quietest_claim_cannot_win_the_whole_utterance() {
        let claims = [
            c("dot2-e", 1000, Some(-39.5)),
            c("dot2-b", 1091, Some(-32.8)),
            c("dot2-d", 1753, Some(-30.5)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(
            d.winner.device, "dot2-b",
            "earliest claim still within 3 dB of the top"
        );
        assert_eq!(d.why, Why::First);
        assert_ne!(
            d.winner.device, "dot2-e",
            "9 dB down must never win on time"
        );
    }

    #[test]
    fn a_simultaneous_but_much_louder_claim_wins() {
        // 40 ms apart (inside TIE_MS), 20 dB louder: the room with the person.
        let claims = [
            c("dot2-e", 1000, Some(-40.0)),
            c("dot2-c", 1040, Some(-20.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-c");
        assert_eq!(d.why, Why::Louder);
        // Negative = the winner arrived LATER (it won on level, not on time).
        assert_eq!(d.margin_ms, Some(-40));
        assert!(d.margin_db.unwrap() >= 19.0);
    }

    #[test]
    fn a_louder_claim_inside_the_tie_window_must_clear_the_margin() {
        // Louder by 2 dB only: not enough evidence to overturn "first".
        let claims = [
            c("dot2-e", 1000, Some(-40.0)),
            c("dot2-c", 1120, Some(-38.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::First);
        assert_eq!(d.margin_ms, Some(120));
    }

    #[test]
    fn identical_claims_fall_back_to_the_fixed_order_and_say_so() {
        let claims = [
            c("dot2-e", 1000, Some(-30.0)),
            c("dot2-c", 1000, Some(-30.0)),
        ];
        let order = vec!["dot2-c".to_string(), "dot2-e".to_string()];
        let d = decide(&claims, &order).unwrap();
        assert_eq!(d.winner.device, "dot2-c", "preferred order decides");
        assert_eq!(d.why, Why::TieBreak);
        // Same inputs in a different array order: same winner (it is a pure
        // function of the claims, not of arrival order).
        let mut reversed = claims.clone();
        reversed.reverse();
        assert_eq!(decide(&reversed, &order).unwrap().winner.device, "dot2-c");
        // No preferred order: lexicographic, still deterministic.
        assert_eq!(decide(&claims, &[]).unwrap().winner.device, "dot2-c");
    }

    #[test]
    fn claims_seconds_apart_are_different_utterances() {
        // Two people in two rooms, 3 s apart (> COALESCE_MS): neither may suppress the other.
        let a = decide(
            &[
                c("dot2-e", 1000, Some(-30.0)),
                c("dot2-d", 4000, Some(-30.0)),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(a.winner.device, "dot2-e");
        assert_eq!(a.considered, 1, "the later utterance is not a competitor");
        let b = decide(&[c("dot2-d", 4000, Some(-30.0))], &[]).unwrap();
        assert_eq!(b.winner.device, "dot2-d");
        assert_eq!(b.why, Why::Solo);
    }

    #[test]
    fn a_claim_without_a_level_never_wins_on_loudness() {
        // When any peer lacks `rms_dbfs`, loudness cannot be compared across
        // peers: do NOT blindly award `Why::Louder`, fall back to arrival time
        // and record `Why::IncompleteFallback`.
        let claims = [c("dot2-e", 1000, None), c("dot2-c", 1010, Some(-20.0))];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);
        // And the reverse: a leader with a level, a follower without one.
        let claims = [c("dot2-e", 1000, Some(-50.0)), c("dot2-c", 1010, None)];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);
        // No levels anywhere: arrival time decides with `IncompleteFallback`,
        // and a true tie falls to the fixed order (`TieBreak`).
        let claims = [c("dot2-e", 1000, None), c("dot2-c", 1600, None)];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);
        let claims = [c("dot2-e", 1000, None), c("dot2-c", 1000, None)];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-c");
        assert_eq!(d.why, Why::TieBreak);
    }

    #[test]
    fn clipped_degraded_low_snr_and_profile_mismatch_are_excluded_from_loudness() {
        // Clipped peer cannot win on loudness even if it is much louder.
        let claims = [
            c("dot2-e", 1000, Some(-12.0)).with_clipped(true),
            c("dot2-c", 1050, Some(-28.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);

        // With 3 peers where 1 is clipped, the remaining 2 clean peers still
        // compare loudness, and the clipped peer cannot win on loudness.
        let claims = [
            c("dot2-a", 1000, Some(-8.0)).with_clipped(true),
            c("dot2-b", 1020, Some(-30.0)),
            c("dot2-c", 1030, Some(-20.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-c");
        assert_eq!(d.why, Why::Louder);

        // Degraded echo_health cannot win on loudness.
        let claims = [
            c("dot2-e", 1000, Some(-15.0)).with_echo_health(Some("degraded".into())),
            c("dot2-c", 1050, Some(-25.0)).with_echo_health(Some("healthy".into())),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);

        // Low SNR (< MIN_SNR_DB) cannot win on loudness.
        let claims = [
            c("dot2-e", 1000, Some(-18.0)).with_snr_db(Some(3.0)),
            c("dot2-c", 1050, Some(-28.0)).with_snr_db(Some(12.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);

        // Mismatched profile_version across peers falls back to IncompleteFallback.
        let claims = [
            c("dot2-e", 1000, Some(-35.0)).with_profile_version(Some("v1".into())),
            c("dot2-c", 1050, Some(-18.0)).with_profile_version(Some("v2".into())),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);
    }

    #[test]
    fn five_speakers_one_answer() {
        // The acceptance case: one phrase, five claims, one winner.
        let claims = [
            c("dot2a", 1000, Some(-52.0)),
            c("dot2-e", 1000, Some(-28.0)),
            c("dot2-d", 1005, Some(-47.0)),
            c("dot2-c", 1010, Some(-55.0)),
            c("dot2-b", 1020, Some(-49.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e", "the room the person is in");
        assert_eq!(d.why, Why::Louder);
        assert_eq!(d.considered, 5);
        // Only one winner, by construction.
        let winners = claims
            .iter()
            .filter(|x| decide(&claims, &[]).unwrap().winner.device == x.device)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn no_claims_no_decision() {
        assert!(decide(&[], &[]).is_none());
    }

    #[test]
    fn three_node_edge_window_produces_symmetric_unique_winner() {
        // A at 0 (-30 dB), B at 1500 (-25 dB), C at 3000 (-20 dB) with COALESCE_MS = 2400.
        // Previously, B's `decide_around` re-filtered around `first_ms = 0`, dropping C
        // and letting BOTH B and C win. Without redundant re-filtering, C is the unique winner.
        let a = c("dot2-a", 0, Some(-30.0));
        let b = c("dot2-b", 1500, Some(-25.0));
        let c_claim = c("dot2-c", 3000, Some(-20.0));
        let all = [a.clone(), b.clone(), c_claim.clone()];

        let da = decide_around(&a, &all, &[]).unwrap();
        let db = decide_around(&b, &all, &[]).unwrap();
        let dc = decide_around(&c_claim, &all, &[]).unwrap();

        assert_ne!(da.winner.device, "dot2-a");
        assert_eq!(db.winner.device, "dot2-c");
        assert_eq!(dc.winner.device, "dot2-c");

        let winners = [(&a, da), (&b, db), (&c_claim, dc)]
            .into_iter()
            .filter(|(local, d)| d.winner.device == local.device)
            .count();
        assert_eq!(winners, 1, "exactly one speaker may win");
    }

    #[test]
    fn arbiter_inbound_retains_sliding_window_and_begin_retrigger_updates_id() {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let arbiter = Arbiter::new(store);

        // No open round: inbound returns no_round, yet retains peer claim in sliding window.
        let peer = c("dot2-e", 10_000, Some(-20.0));
        let resp = arbiter.inbound(peer.clone());
        assert_eq!(resp["state"], "no_round");
        assert_eq!(arbiter.recent_foreign_claims("dot2-d").len(), 1);

        // Re-trigger on the same device within COALESCE_MS updates the round id consistently,
        // and `begin` seeds the compatible fresh foreign claim (`dot2-e`) into the round.
        let id1 = arbiter.begin(c("dot2-d", 10_100, Some(-30.0)));
        assert_eq!(arbiter.claims_so_far(&id1).len(), 2);
        let id2 = arbiter.begin(c("dot2-d", 10_300, Some(-28.0)));
        assert_ne!(id1, id2);
        assert_eq!(arbiter.claims_so_far(&id2).len(), 2);
        assert!(arbiter.claims_so_far(&id1).is_empty());
        arbiter.finish(&id2);
    }

    #[test]
    fn uncalibrated_loudness_win_reports_louder_uncalibrated() {
        // Both uncalibrated: loudness still decides, but `why` is `louder_uncalibrated`.
        let claims = [
            Claim::new("dot2-e", 1000, Some(-32.0)),
            Claim::new("dot2-d", 1300, Some(-20.0)),
        ];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-d");
        assert_eq!(d.why, Why::LouderUncalibrated);

        // Winner calibrated, peer uncalibrated: still `louder_uncalibrated`.
        let mixed = [
            Claim::new("dot2-e", 1000, Some(-32.0)),
            Claim::new("dot2-d", 1300, Some(-20.0)).with_calibrated(true),
        ];
        let d2 = decide(&mixed, &[]).unwrap();
        assert_eq!(d2.winner.device, "dot2-d");
        assert_eq!(d2.why, Why::LouderUncalibrated);
    }

    #[test]
    fn below_loudness_floor_falls_back_to_arrival_time() {
        // Single measured claim at -60.0 dBFS (< -50.0 floor) vs earlier unmeasured claim:
        // loudness is NOT sufficient evidence, so earlier arrival wins as `Why::First`.
        let claims = [c("dot2-e", 1000, None), c("dot2-d", 1010, Some(-60.0))];
        let d = decide(&claims, &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-e");
        assert_eq!(d.why, Why::IncompleteFallback);

        // Two measured claims both below -50.0 dBFS (-55.0 vs -59.0):
        // loudness does NOT overturn arrival time and `tier` does not exclude -59.0.
        let quiet_pair = [
            c("dot2-e", 1000, Some(-59.0)),
            c("dot2-d", 1050, Some(-55.0)),
        ];
        let d2 = decide(&quiet_pair, &[]).unwrap();
        assert_eq!(d2.winner.device, "dot2-e");
        assert_eq!(d2.why, Why::First);
    }

    #[test]
    fn idle_unknown_echo_health_is_comparable_and_kws_beats_passive_asr_on_fallback_171() {
        // Exact #171 field trace: dot2-d (-18.2 dBFS, healthy, KWS "你好小智")
        // arrived 544 ms after dot2-c (-38.2 dBFS, echo_health="unknown", word="").
        let mut dot2_d = Claim::new("dot2-d", 1544, Some(-18.2))
            .with_snr_db(Some(19.9))
            .with_echo_health(Some("healthy".into()))
            .with_word("你好小智");
        dot2_d.calibrated = false;
        let mut dot2_c = Claim::new("dot2-c", 1000, Some(-38.2))
            .with_snr_db(Some(17.4))
            .with_echo_health(Some("unknown".into()));
        dot2_c.calibrated = false;

        let d = decide(&[dot2_c.clone(), dot2_d.clone()], &[]).unwrap();
        assert_eq!(
            d.winner.device, "dot2-d",
            "20 dB louder KWS claim must beat idle unknown peer"
        );
        assert_eq!(d.why, Why::LouderUncalibrated);

        // Even when the passive/ASR peer lacks RMS completely (forcing IncompleteFallback),
        // a spotted KWS claim (`word != ""`) must beat a passive claim (`word == ""`).
        let no_rms_passive =
            Claim::new("dot2-c", 1000, None).with_echo_health(Some("unknown".into()));
        let d_fb = decide(&[no_rms_passive, dot2_d], &[]).unwrap();
        assert_eq!(
            d_fb.winner.device, "dot2-d",
            "KWS claim must beat passive claim on IncompleteFallback"
        );
        assert_eq!(d_fb.why, Why::IncompleteFallback);
    }
}

#[cfg(test)]
mod stale_claim_tests {
    use super::*;

    fn c(device: &str, heard: i64, rms: Option<f32>) -> Claim {
        Claim::new(device, heard, rms).with_calibrated(true)
    }

    /// Live defect, dot2-b 2026-11-15 23:38: at its own wake the node was
    /// handed a peer claim from FOUR SECONDS earlier, the coalesce filter then
    /// dropped the node's own claim as "too late", and it lost to a phrase it
    /// never heard — three times in a row, so the study stayed silent.
    #[test]
    fn an_older_utterances_claim_cannot_steal_this_one() {
        let local = c("dot2-b", 51_065, Some(-18.6));
        let stale = c("dot2-d", 47_701, Some(-37.6));
        let d = decide_around(&local, &[local.clone(), stale.clone()], &[]).unwrap();
        assert_eq!(
            d.winner.device, "dot2-b",
            "the local utterance is what is contested"
        );
        assert_eq!(d.why, Why::Solo);
        assert_eq!(d.considered, 1);
        // The plain set-based call is what used to run: it picks the stale one.
        assert_eq!(
            decide(&[local, stale], &[]).unwrap().winner.device,
            "dot2-d"
        );
    }

    #[test]
    fn the_anchor_keeps_genuine_simultaneous_claims() {
        let local = c("dot2-b", 51_065, Some(-18.6));
        let peer = c("dot2-d", 51_130, Some(-17.1));
        let d = decide_around(&local, &[local.clone(), peer], &[]).unwrap();
        assert_eq!(
            d.winner.device, "dot2-b",
            "1.5 dB is not enough to overturn time"
        );
        assert_eq!(
            d.why,
            Why::First,
            "earliest keeps it when loudness cannot overturn"
        );
        assert_eq!(d.margin_ms, Some(65));
        assert_eq!(d.considered, 2);
    }

    #[test]
    fn sticky_active_room_receives_continuity_bonus_over_hallway() {
        // Living room has an active voice window (session_sticky = true) and
        // is -32.0 dBFS; hallway is -30.0 dBFS. With +4.0 dB sticky bonus,
        // living room's effective level (-28.0) beats hallway (-30.0) by 2 dB?
        // Wait: -28.0 - (-30.0) = 2.0 < LOUDER_DB (3.0), so sticky tiering
        // still prefers the sticky claim in tiebreak/tiering, and at -29.0 vs
        // -30.0 (-25.0 vs -30.0 = 5.0 >= 3.0) it wins outright with SessionSticky!
        let living = c("dot2-a", 10_000, Some(-29.0)).with_session_sticky(true);
        let hallway = c("dot2-b", 9_950, Some(-30.0));
        let d = decide_around(&living, &[living.clone(), hallway], &[]).unwrap();
        assert_eq!(d.winner.device, "dot2-a");
        assert_eq!(d.why, Why::SessionSticky);

        // If user physically walked into the hallway (-18.0 dBFS vs -32.0 dBFS),
        // hallway still wins because 14 dB overwhelms the 4 dB sticky bonus.
        let living_far = c("dot2-a", 10_000, Some(-32.0)).with_session_sticky(true);
        let hallway_near = c("dot2-b", 10_020, Some(-18.0));
        let d2 = decide_around(&living_far.clone(), &[living_far, hallway_near], &[]).unwrap();
        assert_eq!(d2.winner.device, "dot2-b");
        assert_eq!(d2.why, Why::Louder);
    }

    #[test]
    fn passive_near_end_presence_enters_peer_contention_without_kws() {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let arbiter = Arbiter::new(store);
        let now = 50_000i64;
        arbiter
            .record_passive_presence(Claim::new("dot2-b", now, Some(-20.0)).with_calibrated(true));
        let reply =
            arbiter.inbound(Claim::new("dot2-a", now - 50, Some(-35.0)).with_calibrated(true));
        assert_eq!(reply["state"], "passive_presence");
        assert_eq!(reply["claim"]["device"], "dot2-b");
        assert!(arbiter.won_recently("dot2-b", Duration::from_secs(5)));
    }
}
