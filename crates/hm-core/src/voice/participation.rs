//! Session and request transitions. The device's `states` mutex serializes
//! every call; stop_epoch remains a separate audio cancellation mechanism.
use super::{CreditGrant, FollowWindowPolicy, Session, SessionState};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) enum RequestKind {
    First,
    Followup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct RequestTicket {
    pub generation: u64,
    pub request_id: u64,
    pub kind: RequestKind,
}

#[derive(Clone, Copy)]
pub(super) struct TurnTicket {
    pub context: Option<crate::session::ContextTicket>,
    pub request: RequestTicket,
    pub stop_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseReason {
    ExplicitExit,
    Timeout,
    SingleRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    generation: u64,
    revision: u64,
    request: Option<RequestTicket>,
}

#[derive(Default)]
struct Request {
    cancel: std::sync::Arc<crate::task::CancellationScope>,
    production_done: bool,
    aborted_after_last_end: bool,
    close_after_answer: bool,
    answers: HashSet<String>,
    last_answer_end_ms: Option<u64>,
}

#[derive(Default)]
pub(super) struct Participation {
    pub session: Session,
    capture_lease: Option<String>,
    strict_capture: bool,
    capture_reset_pending: bool,
    pub follow_window: FollowWindowPolicy,
    pub last_activity_ms: u64,
    pub last_session_open_ms: u64,
    pub followup_credits: u8,
    pub follow_window_blocked: bool,
    generation: u64,
    revision: u64,
    next_request_id: u64,
    latest: Option<RequestTicket>,
    requests: HashMap<u64, Request>,
    tail_end_ms: u64,
    has_answer_tail: bool,
    pub close_reason: Option<CloseReason>,
    pub opened_by: Option<&'static str>,
    pub last_denied: Option<serde_json::Value>,
    pub close_confirmation: Option<RequestTicket>,
    /// `LEARN-10`: explicit bounded extended wait window while a speaker
    /// vocabulary card is active (never infinite open mic).
    pub learning_wait_ms: Option<u64>,
    pub learning_wait_until_ms: u64,
}

impl Participation {
    /// The capture revocation lease survives activation of this same input.
    /// Only close/reset revokes it; arrival timestamps never grant identity.
    pub fn capture_lease(&mut self) -> String {
        self.capture_lease
            .get_or_insert_with(|| crate::id::next("capture"))
            .clone()
    }

    pub fn reset_capture(&mut self) -> String {
        let lease = crate::id::next("capture");
        self.capture_lease = Some(lease.clone());
        lease
    }

    pub fn revoke_capture(&mut self) -> String {
        self.strict_capture = true;
        self.capture_reset_pending = true;
        let lease = crate::id::next("capture");
        self.capture_lease = Some(lease.clone());
        lease
    }

    pub fn take_capture_reset(&mut self) -> bool {
        std::mem::take(&mut self.capture_reset_pending)
    }

    /// Whether a close has revoked leased tickets (diagnostics/tests).
    #[cfg(test)]
    pub fn capture_revoked(&self) -> bool {
        self.strict_capture
    }

    pub fn accepts_origin(&mut self, origin: &crate::gateway::InputOrigin) -> bool {
        match origin {
            crate::gateway::InputOrigin::Leased { lease, origin, .. } => {
                lease == &self.capture_lease()
                    && match &**origin {
                        crate::gateway::InputOrigin::Audio {
                            capture_epoch,
                            ticket,
                            input_id,
                            start_sample,
                            end_sample,
                        } => {
                            !capture_epoch.is_empty()
                                && !ticket.is_empty()
                                && !input_id.is_empty()
                                && start_sample < end_sample
                        }
                        crate::gateway::InputOrigin::Text { input_id } => !input_id.is_empty(),
                        _ => false,
                    }
            }
            // COMPATIBILITY (2026-11-15, codex-reviewed): explicit Legacy
            // inputs are accepted so the real gateway (no ticket wiring yet)
            // cannot be deafened by a close — one「退下吧」on dot2-d produced
            // ~600–750 `stale_session_event_rejected via=pcm` per MINUTE and
            // permanently killed wake. A close revokes the OLD generation's
            // LEASES; an unticketed new wake is treated as the start of the
            // next session under the compatibility contract. This is NOT the
            // strict D2 guarantee: a late Legacy keyword from pre-close audio
            // can still open a new session (see「Legacy 放行评估」§2 in the
            // codex plan). Bare Audio/Text keep the pre-close semantics.
            crate::gateway::InputOrigin::Legacy => true,
            _ => !self.strict_capture,
        }
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let phase = match self.session.state {
            SessionState::Idle => "closed",
            SessionState::Listening if !self.request_running() && self.has_answer_tail => "tail",
            SessionState::Listening => "active",
        };
        let now = crate::id::now_ms();
        let now_ref = self.last_activity_ms.max(self.tail_end_ms);
        let eff_ms = self.effective_follow_window_ms(now);
        serde_json::json!({
            "state": self.session.label(), "since_ms": self.session.since_ms,
            "generation": self.generation, "phase": phase,
            "opened_by": self.opened_by, "credits": self.followup_credits,
            "tail_deadline_ms": (phase == "tail")
                .then_some(now_ref.saturating_add(eff_ms)),
            "learning_wait": {
                "active": self.learning_wait_active(now),
                "window_ms": self.learning_wait_ms,
                "until_ms": self.learning_wait_until_ms,
            },
            "close_reason": self.close_reason.map(|r| match r {
                CloseReason::ExplicitExit => "explicit_exit", CloseReason::Timeout => "timeout", CloseReason::SingleRequest => "single_request"
            }), "last_denied": self.last_denied,
        })
    }

    pub fn request_for_flag(
        &self,
        flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<RequestTicket> {
        let latest = self.latest?;
        self.requests
            .get(&latest.request_id)
            .filter(|r| std::sync::Arc::ptr_eq(&r.cancel.flag(), flag))
            .map(|_| latest)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Revoked microphone admission invalidates snapshots without cancelling
    /// an unrelated running request or opening another listening window.
    pub fn invalidate_admission_revision(&mut self) {
        self.revision += 1;
    }

    pub fn identity(&self) -> Identity {
        Identity {
            generation: self.generation,
            revision: self.revision,
            request: self.latest,
        }
    }

    pub fn request_started(&self) -> bool {
        self.latest.is_some()
    }
    pub fn request_running(&self) -> bool {
        !self.requests.is_empty()
    }

    pub fn activate(
        &mut self,
        expected: Identity,
        policy: FollowWindowPolicy,
        grant: CreditGrant,
        now: u64,
    ) -> Option<Identity> {
        if self.identity() != expected {
            return None;
        }
        self.revision += 1;
        self.follow_window = policy;
        self.generation += 1;
        self.latest = None;
        self.cancel_all_requests();
        self.follow_window_blocked = false;
        self.close_reason = None;
        self.opened_by = Some("explicit_activation");
        self.close_confirmation = None;
        self.session.open(now);
        self.session.since_ms = now;
        self.last_session_open_ms = now;
        self.last_activity_ms = now;
        self.tail_end_ms = now;
        self.has_answer_tail = false;
        match (policy.credits, grant) {
            (Some(c), CreditGrant::Full) => self.followup_credits = c,
            (Some(_), CreditGrant::One) if self.followup_credits == 0 => self.followup_credits = 1,
            (None, _) => self.followup_credits = 0,
            _ => {}
        }
        Some(self.identity())
    }

    /// When a `wake_ack` finishes playing on a freshly woken session whose
    /// user request has not yet started, refresh the listening anchor from
    /// `play_finished` so slow LAN/Tailscale delivery of the ack does not eat
    /// the wake listening window. Never reopens closed or blocked sessions.
    pub fn refresh_wake_ack_window(&mut self, now: u64) -> bool {
        if self.session.state != SessionState::Listening
            || self.request_started()
            || self.follow_window_blocked
        {
            return false;
        }
        self.last_activity_ms = self.last_activity_ms.max(now);
        self.tail_end_ms = self.tail_end_ms.max(now);
        true
    }

    pub fn available_followup_credit(&self) -> Option<u8> {
        if !self.follow_window.enabled && self.request_started() {
            return None;
        }
        match self.follow_window.credits {
            None => Some(u8::MAX),
            Some(0) if self.request_started() => None,
            Some(_) if self.followup_credits == 0 => None,
            Some(_) => Some(self.followup_credits - 1),
        }
    }

    pub fn spend_followup_credit(&mut self) -> Option<u8> {
        let remaining = self.available_followup_credit()?;
        if self.follow_window.credits.is_some() {
            self.followup_credits = remaining;
        }
        self.revision += 1;
        Some(remaining)
    }

    pub fn debit_control(&mut self, expected: Identity) -> Option<Identity> {
        if self.identity() != expected {
            return None;
        }
        if self.follow_window.credits.is_some() {
            self.followup_credits = self.followup_credits.checked_sub(1)?;
        }
        self.revision += 1;
        Some(self.identity())
    }

    pub fn begin_request(
        &mut self,
        expected: Identity,
        _now: u64,
        debit: bool,
    ) -> Option<RequestTicket> {
        if self.identity() != expected {
            return None;
        }
        if self.session.state != SessionState::Listening
            || self.follow_window_blocked
            || (!self.follow_window.enabled && self.request_started())
        {
            return None;
        }
        if debit && self.latest.is_some() {
            self.spend_followup_credit()?;
        }
        self.revision += 1;
        // Cancel any older active request in this generation so a superseded
        // request cannot complete new production or start new speech.
        for prev in self.requests.values_mut() {
            prev.cancel.cancel();
        }
        // Admission protects in-flight work but never renews the tail.
        if self
            .close_confirmation
            .is_some_and(|r| r.request_id != self.next_request_id)
        {
            self.close_confirmation = None;
        }
        self.next_request_id += 1;
        let ticket = RequestTicket {
            generation: self.generation,
            request_id: self.next_request_id,
            kind: if self.latest.is_none() {
                RequestKind::First
            } else {
                RequestKind::Followup
            },
        };
        self.latest = Some(ticket);
        self.requests.insert(ticket.request_id, Request::default());
        Some(ticket)
    }

    /// An explicit resume can use a human bookmark after the window timed
    /// out. It creates a fresh session when necessary without refilling the
    /// legacy credit budget; ordinary admission cannot use this path.
    pub fn resume_request(&mut self, expected: Identity, now: u64) -> Option<RequestTicket> {
        if self.identity() != expected {
            return None;
        }
        if self.session.state != SessionState::Listening
            || (!self.follow_window.enabled && self.request_started())
        {
            self.activate(expected, self.follow_window, CreditGrant::None, now)?;
        }
        self.begin_request(self.identity(), now, false)
    }

    pub fn request_cancel_scope(
        &self,
        ticket: RequestTicket,
    ) -> Option<std::sync::Arc<crate::task::CancellationScope>> {
        self.contains(ticket)
            .then(|| self.requests[&ticket.request_id].cancel.clone())
    }

    fn cancel_all_requests(&mut self) {
        for request in self.requests.values() {
            request.cancel.cancel();
        }
        self.requests.clear();
    }

    pub fn contains(&self, ticket: RequestTicket) -> bool {
        ticket.generation == self.generation && self.requests.contains_key(&ticket.request_id)
    }

    /// True only when `ticket` is the latest active (non-cancelled) request in
    /// the current session generation.
    pub fn is_active_request(&self, ticket: RequestTicket) -> bool {
        self.latest == Some(ticket)
            && ticket.generation == self.generation
            && self
                .requests
                .get(&ticket.request_id)
                .is_some_and(|r| !r.cancel.flag().load(std::sync::atomic::Ordering::SeqCst))
    }

    pub fn answer_started(&mut self, ticket: RequestTicket, play: &str) {
        if !self.contains(ticket) {
            return;
        }
        self.requests
            .get_mut(&ticket.request_id)
            .unwrap()
            .answers
            .insert(play.into());
    }

    /// Mark a request (e.g. a completed one-shot Home Assistant action) so that
    /// as soon as its confirmation audio finishes playing, the session closes
    /// immediately (`CloseReason::SingleRequest`) instead of opening a
    /// follow-up listening tail.
    pub fn mark_close_after_answer(&mut self, ticket: RequestTicket) {
        if !self.contains(ticket) {
            return;
        }
        if let Some(r) = self.requests.get_mut(&ticket.request_id) {
            r.close_after_answer = true;
        }
    }

    pub fn answer_ended(&mut self, ticket: RequestTicket, play: &str, now: u64) -> bool {
        if !self.contains(ticket) {
            return false;
        }
        let r = self.requests.get_mut(&ticket.request_id).unwrap();
        if !r.answers.remove(play) {
            return false;
        }
        r.last_answer_end_ms = Some(now);
        r.aborted_after_last_end = false;
        self.revision += 1;
        self.settle(ticket, now);
        true
    }

    pub fn answer_aborted(&mut self, ticket: RequestTicket, play: &str, now: u64) -> bool {
        if !self.contains(ticket)
            || !self
                .requests
                .get_mut(&ticket.request_id)
                .unwrap()
                .answers
                .remove(play)
        {
            return false;
        }
        self.requests
            .get_mut(&ticket.request_id)
            .unwrap()
            .aborted_after_last_end = true;
        self.revision += 1;
        self.settle(ticket, now);
        true
    }

    /// Retire only this production. The return value says whether it was
    /// still the current request; an older A may retire without affecting B.
    pub fn finish_request(&mut self, ticket: RequestTicket, now: u64) -> bool {
        if !self.contains(ticket) {
            return false;
        }
        let r = self.requests.get_mut(&ticket.request_id).unwrap();
        if r.production_done {
            return false;
        }
        r.production_done = true;
        self.revision += 1;
        let current = self.latest == Some(ticket);
        self.settle(ticket, now);
        current
    }

    fn settle(&mut self, ticket: RequestTicket, now: u64) {
        let r = &self.requests[&ticket.request_id];
        if !r.production_done || !r.answers.is_empty() {
            return;
        }
        let close_after_answer = r.close_after_answer;
        let ended = if r.aborted_after_last_end {
            None
        } else {
            r.last_answer_end_ms
        };
        self.requests.remove(&ticket.request_id);
        // A is allowed to retire A even while B runs. Only the latest request
        // owns the admission window; A cannot close or move B's window.
        if self.latest != Some(ticket) {
            return;
        }
        if self.close_confirmation.is_some_and(|r| r != ticket) {
            self.close_confirmation = None;
        }
        if !self.follow_window.enabled
            || self.follow_window.credits == Some(0)
            || close_after_answer
        {
            self.close(self.identity(), CloseReason::SingleRequest, now);
        } else if self.session.state == SessionState::Listening {
            if let Some(ended) = ended {
                self.tail_end_ms = ended;
                self.has_answer_tail = true;
                self.last_session_open_ms = ended;
            }
        }
    }

    pub fn close(&mut self, expected: Identity, reason: CloseReason, now: u64) -> bool {
        if self.identity() != expected {
            return false;
        }
        if reason == CloseReason::ExplicitExit {
            if self.close_reason == Some(CloseReason::ExplicitExit) {
                return false;
            }
        } else if self.close_reason.is_some() || self.session.state != SessionState::Listening {
            return false;
        }
        self.revision += 1;
        self.session.close(now);
        self.close_reason = Some(reason);
        self.cancel_all_requests();
        self.close_confirmation = None;
        self.follow_window_blocked = true;
        self.followup_credits = 0;
        self.learning_wait_ms = None;
        self.learning_wait_until_ms = 0;
        self.generation += 1;
        self.revoke_capture();
        true
    }

    pub fn apply_policy(&mut self, policy: FollowWindowPolicy, now: u64) {
        self.revision += 1;
        self.follow_window = policy;
        if let Some(c) = policy.credits {
            self.followup_credits = self.followup_credits.min(c);
        }
        self.close_if_single_finished(now);
    }

    pub fn close_if_single_finished(&mut self, now: u64) {
        if (!self.follow_window.enabled || self.follow_window.credits == Some(0))
            && self.request_started()
            && !self.request_running()
        {
            self.close(self.identity(), CloseReason::SingleRequest, now);
        }
    }

    pub fn cancel_requests(&mut self, now: u64) {
        // Stop/offline ends outstanding production and delivery without
        // borrowing another request's identity or granting a new window.
        let latest = self.latest;
        self.revision += 1;
        self.cancel_all_requests();
        if !self.follow_window.enabled && latest.is_some() {
            self.close(self.identity(), CloseReason::SingleRequest, now);
        }
    }

    pub fn set_learning_wait(&mut self, window_ms: Option<u64>, until_ms: u64) {
        self.learning_wait_ms =
            window_ms.map(|ms| ms.min(crate::tool::learning::MAX_LEARNING_WAIT_MS));
        self.learning_wait_until_ms = if self.learning_wait_ms.is_some() {
            until_ms
        } else {
            0
        };
    }

    pub fn learning_wait_active(&self, now: u64) -> bool {
        self.learning_wait_ms.is_some() && now < self.learning_wait_until_ms
    }

    pub fn effective_follow_window_ms(&self, now: u64) -> u64 {
        if self.learning_wait_active(now) {
            self.follow_window
                .ms
                .max(self.learning_wait_ms.unwrap_or(0))
                .min(crate::tool::learning::MAX_LEARNING_WAIT_MS)
        } else {
            self.follow_window.ms
        }
    }

    pub fn session_open(&self, now: u64, voice_live: bool) -> bool {
        if self.session.state != SessionState::Listening {
            return false;
        }
        if voice_live || self.request_running() {
            return true;
        }
        now.saturating_sub(self.last_activity_ms.max(self.tail_end_ms))
            < self.effective_follow_window_ms(now)
    }

    pub fn session_covers(&self, start: u64, end: u64, now: u64, voice_live: bool) -> bool {
        if !self.follow_window.enabled && self.request_started() {
            return false;
        }
        if voice_live || self.request_running() {
            return self.session.state == SessionState::Listening || self.request_running();
        }
        if self.session.state != SessionState::Listening {
            return false;
        }
        let from = self.last_session_open_ms.max(self.tail_end_ms);
        let eff = self.effective_follow_window_ms(now);
        let opened_at = self.session.since_ms;
        from != 0
            && start >= opened_at
            && end >= from
            && start < from.saturating_add(eff)
            && end.max(now) >= from
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    fn active() -> Participation {
        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                credits: Some(2),
                ..Default::default()
            },
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        p
    }

    #[test]
    fn d1_same_generation_answers_and_production_settle_independently() {
        let mut p = active();
        let a = p.begin_request(p.identity(), 1_000, false).unwrap();
        p.answer_started(a, "A1");
        p.answer_started(a, "A2");
        let b = p.begin_request(p.identity(), 1_000, false).unwrap();
        p.answer_started(b, "B");
        assert!(
            !p.finish_request(a, 2_000),
            "A retires independently of the current B"
        );
        assert!(p.answer_ended(a, "A1", 3_000));
        assert!(p.contains(a), "A2 is still draining");
        assert!(p.answer_ended(a, "A2", 4_000));
        assert!(!p.contains(a));
        assert!(p.contains(b), "A's audio end must not clear B");
        assert!(!p.answer_ended(a, "A2", 5_000));
        assert!(p.answer_ended(b, "B", 49_000));
        assert!(p.contains(b), "production has not ended");
        assert!(p.finish_request(b, 50_000));
        assert!(!p.request_running());
        assert!(p.session_open(57_999, false));
        assert!(!p.session_open(58_000, false));
        assert_eq!(p.followup_credits, 2, "ending audio never refills credits");
    }

    #[test]
    fn d1_old_answer_cannot_close_disabled_new_request_or_move_its_tail() {
        let mut p = active();
        let a = p.begin_request(p.identity(), 1_000, false).unwrap();
        p.answer_started(a, "A");
        let b = p.begin_request(p.identity(), 1_000, false).unwrap();
        p.follow_window.enabled = false;
        p.finish_request(a, 2_000);
        p.answer_ended(a, "A", 60_000);
        assert!(p.contains(b));
        assert!(p.session_open(60_001, false));
        assert_eq!(p.close_reason, None);
        p.finish_request(b, 61_000);
        assert_eq!(p.close_reason, Some(CloseReason::SingleRequest));
        assert!(!p.session_open(61_001, false));
    }

    #[test]
    fn d1_stale_close_snapshot_cannot_close_same_generation_new_request() {
        let mut p = active();
        let a = p.begin_request(p.identity(), 1_000, false).unwrap();
        let stale = p.identity();
        let b = p.begin_request(p.identity(), 1_000, false).unwrap();
        assert_eq!(a.generation, b.generation);
        assert!(!p.close(stale, CloseReason::Timeout, 30_000));
        assert!(p.contains(b));
        let current = p.identity();
        assert!(p.close(current, CloseReason::ExplicitExit, 30_001));
        let closed = p.identity();
        assert!(!p.close(closed, CloseReason::ExplicitExit, 30_002));
        assert_eq!(p.identity(), closed);
        assert!(!p.finish_request(b, 30_003));
    }

    #[test]
    fn d1_timeout_then_explicit_exit_upgrades_once() {
        let mut p = active();
        let open = p.identity();
        assert!(p.close(open, CloseReason::Timeout, 30_000));
        assert!(!p.close(p.identity(), CloseReason::Timeout, 30_001));
        assert!(p.close(p.identity(), CloseReason::ExplicitExit, 30_002));
        assert!(p.follow_window_blocked);
        assert_eq!(p.followup_credits, 0);
        assert!(!p.close(p.identity(), CloseReason::ExplicitExit, 30_003));
    }

    #[test]
    fn d1_activation_admission_exit_timeout_and_no_audio_race_one_snapshot() {
        // Each contender carries the same immutable state identity. After
        // one transition commits, every other conditional operation is stale.
        for _ in 0..64 {
            let mut p = active();
            let request = p.begin_request(p.identity(), 1_000, false).unwrap();
            let expected = p.identity();
            let state = Arc::new(Mutex::new(p));
            let barrier = Arc::new(Barrier::new(5));
            let threads: Vec<_> = (0..5)
                .map(|op| {
                    let state = state.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        let mut p = state.lock().unwrap();
                        match op {
                            0 => p
                                .activate(
                                    expected,
                                    FollowWindowPolicy::default(),
                                    CreditGrant::Full,
                                    30_000,
                                )
                                .is_some(),
                            1 => p.begin_request(expected, 30_000, true).is_some(),
                            2 => p.close(expected, CloseReason::ExplicitExit, 30_000),
                            3 => p.close(expected, CloseReason::Timeout, 30_000),
                            _ => p.finish_request(request, 30_000),
                        }
                    })
                })
                .collect();
            let winners = threads
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>();
            assert_eq!(winners, 1);
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn d1_concurrent_ordinary_admission_spends_exactly_one_credit() {
        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                credits: Some(2),
                ..Default::default()
            },
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        // D2 first question is free; the contenders are genuine followups.
        let first = p.begin_request(p.identity(), 1_000, true).unwrap();
        p.finish_request(first, 1_001);
        let expected = p.identity();
        let state = std::sync::Arc::new(std::sync::Mutex::new(p));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    state
                        .lock()
                        .unwrap()
                        .begin_request(expected, 2_000, true)
                        .is_some()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|w| w.join().ok())
                .filter(|won| *won)
                .count(),
            1
        );
        assert_eq!(state.lock().unwrap().followup_credits, 1);
    }

    #[test]
    fn d1_explicit_resume_after_single_request_close_keeps_exhausted_credits() {
        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                enabled: false,
                ..Default::default()
            },
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        let old = p.begin_request(p.identity(), 2_000, false).unwrap();
        p.finish_request(old, 3_000);
        assert_eq!(p.followup_credits, 0);
        let resumed = p.resume_request(p.identity(), 4_000).unwrap();
        assert!(resumed.generation > old.generation);
        assert_eq!(resumed.kind, RequestKind::First);
        assert_eq!(
            p.followup_credits, 0,
            "resume must not refill the allowance"
        );
    }

    #[test]
    fn begin_request_cancels_and_supersedes_older_active_request() {
        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                credits: Some(2),
                ..Default::default()
            },
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        let r1 = p.begin_request(p.identity(), 1_100, true).unwrap();
        let r1_cancel = p.request_cancel_scope(r1).unwrap();
        assert!(p.is_active_request(r1));
        assert!(!r1_cancel.flag().load(std::sync::atomic::Ordering::SeqCst));

        let r2 = p.begin_request(p.identity(), 1_200, true).unwrap();
        assert_eq!(r1.generation, r2.generation);
        assert!(r2.request_id > r1.request_id);
        assert!(
            r1_cancel.flag().load(std::sync::atomic::Ordering::SeqCst),
            "older request cancellation scope must be cancelled when new request begins"
        );
        assert!(
            !p.is_active_request(r1),
            "superseded request must no longer be active"
        );
        assert!(p.is_active_request(r2));
    }

    #[test]
    fn learn10_explicit_bounded_learning_wait_extends_and_clears_on_close() {
        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                credits: Some(2),
                ..Default::default()
            },
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        let req = p.begin_request(p.identity(), 1_000, false).unwrap();
        p.answer_started(req, "P1");
        assert!(p.finish_request(req, 1_500));
        assert!(p.answer_ended(req, "P1", 10_000));

        // Ordinary 9s follow-up window closes at +9_000
        assert!(p.session_open(18_999, false));
        assert!(!p.session_open(19_000, false));

        // LEARN-10: active learning wait extends to 25s (bounded, never infinite)
        p.set_learning_wait(Some(25_000), 130_000);
        assert!(p.learning_wait_active(25_000));
        assert!(
            p.session_open(25_000, false),
            "15s after prompt must stay open"
        );
        assert!(p.session_open(34_999, false));
        assert!(
            !p.session_open(35_000, false),
            "must close after bounded 25s window"
        );

        // Ceiling clamps excessive wait values to MAX_LEARNING_WAIT_MS (30s)
        p.set_learning_wait(Some(999_999), 200_000);
        assert_eq!(
            p.effective_follow_window_ms(20_000),
            crate::tool::learning::MAX_LEARNING_WAIT_MS
        );

        // Explicit exit clears learning wait state
        assert!(p.close(p.identity(), CloseReason::ExplicitExit, 20_000));
        assert!(!p.learning_wait_active(20_000));
        assert!(p.learning_wait_ms.is_none());
    }

    #[test]
    fn rc1_superseded_pre_tool_stream_does_not_poison_post_tool_answer_tail() {
        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                credits: Some(2),
                ..Default::default()
            },
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        let req = p.begin_request(p.identity(), 1_000, false).unwrap();
        // Pre-tool preamble stream starts, then post-tool answer starts while preamble is superseded
        p.answer_started(req, "preamble-play");
        p.answer_started(req, "post-tool-answer");
        assert!(p.answer_aborted(req, "preamble-play", 3_000));
        assert!(p.finish_request(req, 5_000));
        assert!(p.answer_ended(req, "post-tool-answer", 15_000));
        // The completed post-tool answer must own tail_end_ms (15_000 + 9_000 = 24_000)
        assert!(
            p.session_open(23_999, false),
            "post-tool answer completion must grant full follow-up tail window"
        );
        assert!(!p.session_open(24_000, false));
    }

    #[test]
    fn wake_ack_refresh_extends_open_listening_window_without_reopening_closed() {
        assert_eq!(super::super::settings::ACK_SETTLE_MS, 2_500);

        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy::default(),
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        // Slow LAN/Tailscale wake_ack finishes 7.5 s after wake -> listening
        // window refreshes from 8_500 so user still gets a full 9 s window.
        assert!(p.refresh_wake_ack_window(8_500));
        assert!(p.session_open(17_499, false));
        assert!(!p.session_open(17_500, false));

        // Once a request has started, wake_ack finish must NOT mutate tail_end_ms.
        let _req = p.begin_request(p.identity(), 10_000, false).unwrap();
        assert!(!p.refresh_wake_ack_window(12_000));

        // On an explicitly closed session, wake_ack finish must NEVER reopen.
        assert!(p.close(p.identity(), CloseReason::ExplicitExit, 12_500));
        assert!(!p.refresh_wake_ack_window(13_000));
        assert!(!p.session_open(13_000, false));
    }

    #[test]
    fn p1_07_session_covers_admits_speech_starting_before_play_finished_and_rejects_pre_session() {
        let mut p = Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                ms: 9_000,
                ..Default::default()
            },
            CreditGrant::Full,
            1_000,
        )
        .unwrap();
        let req = p.begin_request(p.identity(), 1_500, false).unwrap();
        p.answer_started(req, "ans-1");
        assert!(p.finish_request(req, 4_000));
        assert!(p.answer_ended(req, "ans-1", 5_000));

        // Speech starts 150 ms before PlayFinished (4_850 >= opened_at 1_000)
        // and continues into the follow window (end 5_100 >= from 5_000) -> ADMITTED.
        assert!(p.session_covers(4_850, 5_100, 5_100, false));
        // Speech that started AND ended before PlayFinished (end 4_950 < 5_000) -> REJECTED.
        assert!(!p.session_covers(4_600, 4_950, 5_100, false));
        // Speech that started before session opened (start 900 < opened_at 1_000) -> REJECTED.
        assert!(!p.session_covers(900, 5_100, 5_100, false));
        // Speech starting after the follow-up window expires (5_000 + 9_000 = 14_000) -> REJECTED.
        assert!(!p.session_covers(14_000, 14_200, 14_200, false));
    }
}
