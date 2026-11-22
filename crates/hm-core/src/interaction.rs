//! The interaction matrix (R1g): ONE policy function for every device fact
//! that could become a turn, a stop, or nothing.
//!
//! The user's rule (2026-11-12): **half duplex means the wake word first**.
//! Direct near-end speech is a full-duplex ability; it must never create a
//! command on a half-duplex link. The matrix is data, not a chain of `if`s
//! in the handlers — `on_keyword`, the ASR wake path and `on_barge_in` all
//! ask [`decide`] and then execute its verdict.
//!
//! | duplex | trigger      | content playing | honor | stop | ack | reason |
//! |--------|--------------|-----------------|-------|------|-----|--------|
//! | half   | Keyword      | any             | yes   | yes  | no  |        |
//! | half   | Keyword      | none            | yes   | yes  | no  | (ack belongs to the ASR confirmation) |
//! | half   | WakeWord     | any             | yes   | yes  | no  |        |
//! | half   | WakeWord     | none            | yes   | yes  | yes |        |
//! | half   | DirectNearEnd| voice/media     | no    | no   | no  | `direct_barge_suppressed_half_duplex` |
//! | half   | DirectNearEnd| none            | no    | no   | no  | `direct_barge_ignored_idle` |
//! | full   | Keyword      | any             | yes   | yes  | no  |        |
//! | full   | Keyword      | none            | yes   | yes  | no  | (ack belongs to the ASR confirmation) |
//! | full   | WakeWord     | any             | yes   | no   | no  | `full_duplex_wake_keeps_audio` |
//! | full   | WakeWord     | none            | yes   | no   | yes |        |
//! | full   | DirectNearEnd| voice/media     | yes   | yes  | no  |        |
//! | full   | DirectNearEnd| none            | no    | no   | no  | `direct_barge_ignored_idle` |
//!
//! Two cells deserve their reasons written down:
//!
//! - **`stop` means "invalidate the turn in flight", not just "there is audio".**
//!   A wake word that arrives while the answer is still SYNTHESIZING (no play
//!   on the wire yet) must cancel that work too — otherwise the abandoned
//!   answer starts speaking a second later (device test
//!   `utterance_stop_during_pending_synthesis_new_answer_plays_once`). So the
//!   half-duplex wake/keyword cells stop unconditionally; the ring and the log
//!   show whether anything was actually playing (`ack` ⇔ nothing was).
//! - **`ack` only when nothing was playing, and never on the keyword path.**
//!   The ack is a VOICE play, and a voice play closes the half-duplex mic
//!   gate — playing it at the keyword hit swallowed the rest of the sentence
//!   ("你好高达，讲个故事" lost 「讲个故事」: the phrase is spotted ~1 s in).
//!   A bare wake's "我在" therefore belongs to the ASR confirmation, which
//!   knows whether a command followed (`is_greeting_only(rest)`). The keyword
//!   rows carry `ack: false` for that reason; the wake-word rows keep the rule
//!   "safe only when nothing was playing".
//!
//! `full` is unreachable in production today (firmware capability is `half`,
//! and the gateway reports `effective: "half"` honestly). The rows exist and
//! are tested so R3b switches behavior by negotiation, not by a code fork.

/// The link's EFFECTIVE duplex — never the user's request. A half-capable
/// device that was asked for `full` keeps `Half` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duplex {
    Half,
    Full,
}

impl Duplex {
    pub fn from_effective(s: &str) -> Self {
        // "unknown" (no report yet) is half: the safe direction is "the wake
        // word is required", never "any voice may interrupt".
        if s == "full" {
            Duplex::Full
        } else {
            Duplex::Half
        }
    }
}

/// What a spotted word MEANS (R1j/P1). The vocabulary itself is
/// configuration (`config::ReflexConfig`); this is the policy-side identity
/// the matrix reasons about, so adding a phrase never adds a policy path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordIntent {
    Wake,
    Retreat,
    Pause,
    Continue,
    /// M3: advance the media queue (never re-search).
    Next,
    /// M3: stop and DISCARD the current content (the pause counterpart).
    Stop,
}

/// What produced the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The device-side spotter recognized a configured word (`on_keyword`).
    /// The intent is resolved from the reflex table BEFORE the matrix runs.
    Keyword(KeywordIntent),
    /// ASR matched a wake word in a transcribed segment.
    WakeWord,
    /// The device's AEC proved near-end speech during its own playback.
    DirectNearEnd,
}

/// What is live on the device right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Content {
    pub voice: bool,
    pub media: bool,
}

impl Content {
    pub const NONE: Content = Content {
        voice: false,
        media: false,
    };
    pub fn voice() -> Self {
        Content {
            voice: true,
            media: false,
        }
    }
    pub fn media() -> Self {
        Content {
            voice: false,
            media: true,
        }
    }
    pub fn any(self) -> bool {
        self.voice || self.media
    }
}

/// The matrix's verdict. `reason` is `""` for a plain honour, otherwise the
/// stable tag written to the event log (and shown by the console).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Open the follow window (the fact becomes a wake / an interruption).
    pub honor: bool,
    /// Stop every channel and cancel the in-flight turn. R1j/P1: false for
    /// Pause/Continue — their handlers execute the intent through the SAME
    /// path the text layer uses (`program_pause` / `program_resume`), which
    /// owns the audio; a blind stop would lose the bookmark.
    pub stop: bool,
    /// Speak the local ack (only ever true when nothing was playing).
    pub ack: bool,
    /// The intent the handler must execute (None = the wake path).
    pub intent: Option<KeywordIntent>,
    pub reason: &'static str,
}

pub const REASON_DIRECT_SUPPRESSED_HALF: &str = "direct_barge_suppressed_half_duplex";
pub const REASON_DIRECT_IDLE: &str = "direct_barge_ignored_idle";
pub const REASON_FULL_WAKE_KEEPS_AUDIO: &str = "full_duplex_wake_keeps_audio";

/// The table. Every cell is asserted in `tests::the_matrix_is_the_table`.
pub fn decide(duplex: Duplex, trigger: Trigger, content: Content) -> Decision {
    let playing = content.any();
    match (duplex, trigger) {
        // A wake word / the phrase means "I am taking over": cancel whatever
        // the old turn is still doing, even if its first audio has not
        // reached the wire yet. NO ack: see the note above — the spoken ack
        // would close the mic gate on the command said in the same breath.
        (_, Trigger::Keyword(KeywordIntent::Wake)) => Decision {
            honor: true,
            stop: true,
            ack: false,
            intent: None,
            reason: "",
        },
        // R1j/P1: the command words. None of them opens a window (nobody
        // expects a follow-up after 暂停), and none speaks an ack. Their
        // preconditions live in the handler (session/bookmark state), the
        // VERDICT lives here.
        (_, Trigger::Keyword(KeywordIntent::Retreat)) => Decision {
            honor: false,
            stop: false,
            ack: false,
            intent: Some(KeywordIntent::Retreat),
            reason: "",
        },
        (_, Trigger::Keyword(KeywordIntent::Pause)) => Decision {
            honor: false,
            stop: false,
            ack: false,
            intent: Some(KeywordIntent::Pause),
            reason: "",
        },
        (_, Trigger::Keyword(KeywordIntent::Continue)) => Decision {
            honor: false,
            stop: false,
            ack: false,
            intent: Some(KeywordIntent::Continue),
            reason: "",
        },
        // M3 media words: the handler owns the audio (a blind stop here would
        // lose the session/position for pause and the queue for next).
        (_, Trigger::Keyword(KeywordIntent::Next)) => Decision {
            honor: false,
            stop: false,
            ack: false,
            intent: Some(KeywordIntent::Next),
            reason: "",
        },
        (_, Trigger::Keyword(KeywordIntent::Stop)) => Decision {
            honor: false,
            stop: false,
            ack: false,
            intent: Some(KeywordIntent::Stop),
            reason: "",
        },
        (Duplex::Half, Trigger::WakeWord) => Decision {
            honor: true,
            stop: true,
            ack: !playing,
            intent: None,
            reason: "",
        },
        (Duplex::Full, Trigger::WakeWord) => Decision {
            honor: true,
            stop: false,
            ack: !playing,
            intent: None,
            reason: REASON_FULL_WAKE_KEEPS_AUDIO,
        },
        (Duplex::Half, Trigger::DirectNearEnd) => Decision {
            honor: false,
            stop: false,
            ack: false,
            intent: None,
            reason: if playing {
                REASON_DIRECT_SUPPRESSED_HALF
            } else {
                REASON_DIRECT_IDLE
            },
        },
        (Duplex::Full, Trigger::DirectNearEnd) => Decision {
            honor: playing,
            stop: playing,
            ack: false,
            intent: None,
            reason: if playing { "" } else { REASON_DIRECT_IDLE },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents() -> Vec<Content> {
        vec![
            Content::NONE,
            Content::voice(),
            Content::media(),
            Content {
                voice: true,
                media: true,
            },
        ]
    }

    /// Every cell of the table above, verbatim. A cell changing here is a
    /// product decision, not a refactor.
    #[test]
    fn the_matrix_is_the_table() {
        let triggers = [
            Trigger::Keyword(KeywordIntent::Wake),
            Trigger::Keyword(KeywordIntent::Retreat),
            Trigger::Keyword(KeywordIntent::Pause),
            Trigger::Keyword(KeywordIntent::Continue),
            Trigger::Keyword(KeywordIntent::Next),
            Trigger::Keyword(KeywordIntent::Stop),
            Trigger::WakeWord,
            Trigger::DirectNearEnd,
        ];
        for duplex in [Duplex::Half, Duplex::Full] {
            for trigger in triggers {
                for content in contents() {
                    let d = decide(duplex, trigger, content);
                    let playing = content.any();
                    let (honor, stop, ack, reason) = match (duplex, trigger) {
                        (_, Trigger::Keyword(KeywordIntent::Wake)) => (true, true, false, ""),
                        (_, Trigger::Keyword(KeywordIntent::Retreat)) => (false, false, false, ""),
                        (_, Trigger::Keyword(KeywordIntent::Pause)) => (false, false, false, ""),
                        (_, Trigger::Keyword(KeywordIntent::Continue)) => (false, false, false, ""),
                        (_, Trigger::Keyword(KeywordIntent::Next)) => (false, false, false, ""),
                        (_, Trigger::Keyword(KeywordIntent::Stop)) => (false, false, false, ""),
                        (Duplex::Half, Trigger::WakeWord) => (true, true, !playing, ""),
                        (Duplex::Full, Trigger::WakeWord) => {
                            (true, false, !playing, REASON_FULL_WAKE_KEEPS_AUDIO)
                        }
                        (Duplex::Half, Trigger::DirectNearEnd) => (
                            false,
                            false,
                            false,
                            if playing {
                                REASON_DIRECT_SUPPRESSED_HALF
                            } else {
                                REASON_DIRECT_IDLE
                            },
                        ),
                        (Duplex::Full, Trigger::DirectNearEnd) => (
                            playing,
                            playing,
                            false,
                            if playing { "" } else { REASON_DIRECT_IDLE },
                        ),
                    };
                    assert_eq!(
                        (d.honor, d.stop, d.ack, d.reason),
                        (honor, stop, ack, reason),
                        "cell duplex={duplex:?} trigger={trigger:?} content={content:?}"
                    );
                }
            }
        }
    }

    /// The half-duplex contract in one property: no DIRECT near-end fact ever
    /// stops audio or opens a window, whatever is playing.
    #[test]
    fn half_duplex_never_honours_a_direct_near_end() {
        for content in contents() {
            let d = decide(Duplex::Half, Trigger::DirectNearEnd, content);
            assert!(!d.honor && !d.stop && !d.ack);
        }
    }

    /// The wake-word contract: half duplex always stops what plays, full
    /// duplex never cuts its own answer just because its name was called.
    #[test]
    fn the_wake_word_stops_in_half_and_never_in_full() {
        for content in [
            Content::voice(),
            Content::media(),
            Content {
                voice: true,
                media: true,
            },
        ] {
            assert!(decide(Duplex::Half, Trigger::WakeWord, content).stop);
            assert!(!decide(Duplex::Full, Trigger::WakeWord, content).stop);
        }
    }

    /// `unknown` (a device that has not reported yet) must be treated as
    /// half: the safe direction is "wake word required".
    #[test]
    fn an_unreported_duplex_is_half() {
        assert_eq!(Duplex::from_effective("unknown"), Duplex::Half);
        assert_eq!(Duplex::from_effective(""), Duplex::Half);
        assert_eq!(Duplex::from_effective("full"), Duplex::Full);
    }
}
