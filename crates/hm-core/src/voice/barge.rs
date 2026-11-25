use super::BARGE_HEAD_MAX_SAMPLES;

#[derive(Default)]
pub enum Barge {
    #[default]
    Idle,
    Stopped {
        until_ms: u64,
        guard_until_ms: u64,
        head: Vec<i16>,
        head_sample_rate: u32,
        head_provenance: crate::mic::CaptureProvenance,
        head_until_ms: u64,
    },
    Feeding {
        until_ms: u64,
        head: Option<(Vec<i16>, u32, crate::mic::CaptureProvenance)>,
        head_until_ms: u64,
        utterance_from_ms: u64,
        segment_open: bool,
        last_loud_ms: u64,
    },
}

impl Barge {
    /// Transition to `Stopped` with the retained preroll head.
    #[cfg(test)]
    pub fn stop(
        &mut self,
        until_ms: u64,
        guard_until_ms: u64,
        head: Vec<i16>,
        head_sample_rate: u32,
        head_provenance: crate::mic::CaptureProvenance,
        head_until_ms: u64,
    ) {
        *self = Barge::Stopped {
            until_ms,
            guard_until_ms,
            head,
            head_sample_rate,
            head_provenance,
            head_until_ms,
        };
    }

    /// Advance or expire the barge-in state machine for `now`.
    pub fn expire(&mut self, now: u64) {
        let next = match self {
            Barge::Idle => return,
            Barge::Stopped {
                until_ms,
                guard_until_ms,
                head,
                head_sample_rate,
                head_provenance,
                head_until_ms,
            } => {
                if now >= *until_ms {
                    Some(Barge::Idle)
                } else if now >= *guard_until_ms {
                    let retained = if now >= *head_until_ms {
                        None
                    } else {
                        Some((
                            std::mem::take(head),
                            *head_sample_rate,
                            std::mem::take(head_provenance),
                        ))
                    };
                    Some(Barge::Feeding {
                        until_ms: *until_ms,
                        head: retained,
                        head_until_ms: *head_until_ms,
                        utterance_from_ms: *guard_until_ms,
                        segment_open: false,
                        last_loud_ms: 0,
                    })
                } else {
                    None
                }
            }
            Barge::Feeding {
                until_ms,
                head,
                head_until_ms,
                utterance_from_ms,
                ..
            } => {
                if now >= *until_ms || now < *utterance_from_ms {
                    Some(Barge::Idle)
                } else {
                    if now >= *head_until_ms {
                        *head = None;
                    }
                    None
                }
            }
        };
        if let Some(s) = next {
            *self = s;
        }
    }

    pub fn is_window_open(&self, now: u64) -> bool {
        match self {
            Barge::Idle => false,
            Barge::Stopped { until_ms, .. } | Barge::Feeding { until_ms, .. } => now < *until_ms,
        }
    }

    pub fn is_preroll_held(&self) -> bool {
        !matches!(self, Barge::Idle)
    }

    /// Take the retained head ONLY in `Feeding` (stale head splicing across
    /// expired episodes is unrepresentable).
    pub fn take_head(&mut self) -> Option<(Vec<i16>, u32, crate::mic::CaptureProvenance)> {
        if let Barge::Feeding {
            head, segment_open, ..
        } = self
        {
            if let Some(h) = head.take() {
                *segment_open = true;
                return Some(h);
            }
        }
        None
    }

    pub fn append_tail(
        &mut self,
        mono: &[i16],
        sample_rate: u32,
        provenance: &crate::mic::CaptureProvenance,
    ) {
        if let Barge::Stopped {
            head,
            head_sample_rate,
            head_provenance,
            ..
        } = self
        {
            let room = BARGE_HEAD_MAX_SAMPLES.saturating_sub(head.len());
            if room > 0 {
                *head_sample_rate = sample_rate;
                head_provenance.append(provenance.slice(0, room.min(mono.len())));
                head.extend(mono.iter().take(room));
            }
        }
    }

    pub fn note_loud(&mut self, now: u64) {
        if let Barge::Feeding { last_loud_ms, .. } = self {
            *last_loud_ms = now;
        }
    }

    /// Close a post-barge-in segment. Returns `(tail_only, barge_served)`.
    pub fn finish_segment(&mut self) -> (bool, bool) {
        if let Barge::Feeding {
            segment_open,
            last_loud_ms,
            ..
        } = self
        {
            let tail_only = *segment_open && *last_loud_ms == 0;
            if tail_only {
                *segment_open = false;
                *last_loud_ms = 0;
                return (true, false);
            }
            *self = Barge::Idle;
            return (false, true);
        }
        (false, false)
    }

    pub fn reset(&mut self) {
        *self = Barge::Idle;
    }
}
