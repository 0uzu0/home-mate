use super::*;

#[cfg(test)]
mod intent_tests {
    use super::*;

    #[test]
    fn greetings_and_noise_are_classified() {
        for rest in [
            "",
            "你好",
            "你好。",
            "您好！",
            "嗨",
            "哈喽～",
            "喂",
            "hi",
            "HELLO",
        ] {
            assert!(is_greeting_only(rest), "{rest:?} should be greeting-only");
        }
        for rest in ["今天周几", "放首歌", "高达你好吗"] {
            assert!(!is_greeting_only(rest), "{rest:?} is a real command");
        }
    }

    #[test]
    fn retreat_matches_short_commands_only() {
        for t in [
            "退下",
            "退下吧",
            "你退下吧",
            "退一下吧。",
            "休息吧",
            "没事了",
            "不用了，谢谢",
        ] {
            assert!(is_retreat(t), "{t:?} should be retreat");
        }
        // A keyword buried in a real request must NOT match.
        for t in ["帮我找一首叫退下的歌", "查一下退下这个词语的意思"] {
            assert!(!is_retreat(t), "{t:?} is a real request, not retreat");
        }
    }
}

#[cfg(test)]
mod in_window_tests {
    use super::*;

    fn wakes() -> Vec<String> {
        vec!["你好高达".into()]
    }

    /// R1h fix: the wake phrase heard again inside the open window is not a
    /// command. KWS opens the window 1-3 s before the ASR transcript; without
    /// this the wake phrase ran a greeting turn whose answer closed the mic
    /// gate and swallowed the question that followed (device log 19:17).
    #[test]
    fn a_bare_wake_inside_the_window_is_not_a_command() {
        for t in [
            "你好，高达。",
            "你好高达",
            "你好，高大。",
            "你好，高达，你好",
        ] {
            assert_eq!(
                VoicePipeline::in_window_command(t, &wakes()),
                InWindow::Bare,
                "{t} is the wake phrase, not a request"
            );
        }
    }

    #[test]
    fn a_wake_word_at_the_head_of_a_command_is_prologue() {
        assert_eq!(
            VoicePipeline::in_window_command("你好，高达，讲个故事", &wakes()),
            InWindow::Command("讲个故事".into())
        );
        // ASR variants of the wake word are the same prologue.
        assert_eq!(
            VoicePipeline::in_window_command("你好，高大，今天天气怎么样？", &wakes()),
            InWindow::Command("今天天气怎么样？".into())
        );
    }

    #[test]
    fn a_plain_command_passes_through_untouched() {
        assert_eq!(
            VoicePipeline::in_window_command("今天沈阳天气怎么样？", &wakes()),
            InWindow::Command("今天沈阳天气怎么样？".into())
        );
        // A wake word in the MIDDLE is part of the sentence: the tail rule
        // (same as the idle path) makes what follows the request.
        assert_eq!(
            VoicePipeline::in_window_command("把你好高达的音量调大", &wakes()),
            InWindow::Command("的音量调大".into())
        );
    }
}

#[cfg(test)]
mod stop_intent_tests {
    use super::*;

    #[test]
    fn the_negation_wins_over_the_stop_word() {
        let stop = default_stop_words();
        let cont = default_continue_words();
        for t in ["不要停", "别停", "不用停", "不要停下"] {
            assert!(
                continue_match(t, &cont),
                "{t:?} must be a keep-going request"
            );
            // Both matchers fire on the substring "停"; the CALLER tests
            // continue first. Pin that the continue list really covers it.
            assert!(stop_match(t, &stop), "premise: {t:?} contains a stop word");
        }
        for t in ["停", "停下", "停止", "别说了"] {
            assert!(stop_match(t, &stop), "{t:?} must be a stop request");
            assert!(!continue_match(t, &cont), "{t:?} is not keep-going");
        }
    }

    /// User decision 2026-11-15: 「停止播放」 is not an "interrupt this
    /// answer" word. It ends the session (retreat) on BOTH entry points —
    /// the spotter's intent and the transcript fallback — because it used to
    /// mean two different things depending on who heard it (live dot2-e
    /// 2026-11-14: "nothing active on this device", once discarding a
    /// bookmark).
    ///
    /// It is NOT true that `stop_match("停止播放")` is false: the shorter
    /// word 「停止」 is still a stop word and matches inside it. What makes
    /// the word mean one thing is the ORDER at the call sites — retreat is
    /// tested before stop in both transcript paths (voice.rs:5683 followup,
    /// voice.rs:5769 wake residue), and the spotter's intent is now Retreat.
    /// The end-to-end proof is `stop_playback_ends_the_session_not_the_answer`
    /// in tests/gateway_voice.rs; this test pins the tables.
    #[test]
    fn stop_playback_means_retreat_on_both_entry_points() {
        let stop = default_stop_words();
        let retreat = default_retreat_words();
        assert!(
            !stop.iter().any(|w| w == "停止播放"),
            "停止播放 must not be in the stop list"
        );
        assert!(
            retreat.iter().any(|w| w == "停止播放"),
            "停止播放 must be a retreat word"
        );
        assert!(retreat_match("停止播放", &retreat));
        // 停 / 别停 keep their meaning.
        assert!(stop_match("停", &stop));
        assert!(stop_match("别说了", &stop));

        // The spotter table carries the same meaning (one word, one intent).
        let cfg = crate::config::ReflexConfig::default();
        let word = cfg
            .words
            .iter()
            .find(|w| w.text == "停止播放")
            .expect("the default reflex table keeps 停止播放");
        assert_eq!(word.intent, crate::config::ReflexIntent::Retreat);
    }

    #[test]
    fn a_stop_word_inside_a_real_request_does_not_match() {
        let stop = default_stop_words();
        // Length guard: a stop word buried in a sentence is content.
        assert!(!stop_match("帮我找一首叫停下来的歌", &stop));
        assert!(!stop_match("停车场的灯关掉", &stop));
        // Short but dominated by content: the word is not a command here.
        assert!(
            !stop_match("放首叫停的歌", &stop),
            "a title is not a command"
        );
        assert!(
            stop_match("你先停一下", &stop),
            "an imperative with a particle still matches"
        );
    }

    #[test]
    fn a_stop_with_a_real_request_still_answers_the_request() {
        let stop = default_stop_words();
        // "停，换个短的": stop happens, the remainder is a new request.
        let rest = stop_remainder("停，换个短的", &stop).expect("a stop is recognized");
        assert_eq!(rest, "换个短的");
        assert!(remainder_is_content(&rest), "the remainder must still run");
        // Particle-only remainders are the whole utterance.
        for t in ["停", "停下吧", "停一下", "别说了"] {
            let rest = stop_remainder(t, &stop).unwrap_or_else(|| panic!("{t:?} must be a stop"));
            assert!(
                !remainder_is_content(&rest),
                "{t:?} leaves nothing to answer"
            );
        }
        // The longer stop word wins, so "停止" never leaves "止" behind.
        assert_eq!(stop_remainder("停止", &stop).as_deref(), Some(""));
        // 「停止播放」 leaves "播放" behind — which is exactly why the retreat
        // check must stay BEFORE the stop check (2026-11-15): the word is a
        // session end, not "stop + a request about playback".
        assert_eq!(stop_remainder("停止播放", &stop).as_deref(), Some("播放"));
    }

    #[test]
    fn a_bookmark_offset_is_never_inside_a_character() {
        let text = "第一句话。第二句话。";
        // 3-byte characters: offset 4 is inside 一.
        let fixed = VoicePipeline::clamp_boundary(text, 4);
        assert!(text.is_char_boundary(fixed));
        assert_eq!(fixed, 3);
        // Beyond the end clamps to the end, and the end is a boundary.
        assert_eq!(VoicePipeline::clamp_boundary(text, 999), text.len());
        assert_eq!(VoicePipeline::clamp_boundary("", 5), 0);
        // A valid boundary is left alone.
        assert_eq!(VoicePipeline::clamp_boundary(text, 12), 12);
    }

    #[test]
    fn the_last_sentence_is_what_the_user_just_said() {
        // A barge-in transcript can start with the tail of the answer we were
        // speaking (the retained head). The intent is in the LAST sentence.
        assert_eq!(last_sentence("……然后他就走了。暂停。"), "暂停。");
        assert_eq!(last_sentence("暂停。"), "暂停。");
        assert_eq!(last_sentence("  停  "), "停");
        assert_eq!(last_sentence("第一句。第二句！第三句"), "第三句");
        assert_eq!(last_sentence(""), "");
        // The head does not change a single-sentence command.
        assert_eq!(last_sentence("讲个故事。"), "讲个故事。");
        // Trailing punctuation only: still the same sentence.
        assert_eq!(last_sentence("暂停。 "), "暂停。");
    }

    #[test]
    fn our_own_cue_words_are_stripped_from_the_head_only() {
        assert_eq!(
            strip_leading_phrase("在呢。今天几号", "在呢").as_deref(),
            Some("今天几号")
        );
        assert_eq!(strip_leading_phrase("在呢", "在呢").as_deref(), Some(""));
        assert_eq!(
            strip_leading_phrase("嗯嗯，在呢。今天几号", "在呢"),
            None,
            "only a head match"
        );
        assert_eq!(strip_leading_phrase("今天几号", "在呢"), None);
        // Punctuation/spacing inside the transcript does not defeat it.
        assert_eq!(
            strip_leading_phrase("在 呢，今天几号", "在呢").as_deref(),
            Some("今天几号")
        );
        assert_eq!(strip_leading_phrase("在呢。今天几号", ""), None);
    }
}

#[cfg(test)]
mod filler_tests {
    use super::*;

    fn cfg() -> FillerSettings {
        FillerSettings::default()
    }

    #[test]
    fn the_transcript_picks_the_scene() {
        assert_eq!(filler_scene("给我讲个故事"), "story");
        assert_eq!(filler_scene("讲个童话吧"), "story");
        assert_eq!(filler_scene("把灯关掉"), "device");
        assert_eq!(filler_scene("音量小一点"), "device");
        assert_eq!(filler_scene("今天天气怎么样"), "search");
        assert_eq!(filler_scene("查一下新闻"), "search");
        assert_eq!(filler_scene("现在几点了"), "thinking");
        // Story wins over the device words inside it (A's order).
        assert_eq!(filler_scene("讲个关于台灯的故事"), "story");
    }

    #[test]
    fn an_empty_scene_disables_only_that_scene() {
        let mut c = cfg();
        c.story.clear();
        assert!(
            c.pick("讲个故事", 1).is_none(),
            "empty story list = no story cue"
        );
        assert!(
            c.pick("现在几点了", 1).is_some(),
            "other scenes must not fall back to a disabled one, nor be disabled by it"
        );
        assert!(
            c.pick("讲个故事", 1).is_none() && c.pick("讲个故事", 2).is_none(),
            "no fallback into another scene"
        );
    }

    #[test]
    fn one_phrase_per_turn_rotating_within_the_scene() {
        let mut c = cfg();
        c.thinking = vec!["我想想。".into(), "我看看。".into()];
        let picks: Vec<String> = (0..4)
            .map(|seq| c.pick("现在几点", seq).unwrap().1)
            .collect();
        assert_eq!(picks, vec!["我想想。", "我看看。", "我想想。", "我看看。"]);
        // The scene is carried with the phrase (the console shows both).
        assert_eq!(c.pick("现在几点", 0).unwrap().0, "thinking");
    }

    #[test]
    fn every_configured_phrase_joins_the_cue_cache() {
        let c = cfg();
        let all = c.all_phrases();
        assert_eq!(all.len(), 4, "defaults: one phrase per scene: {all:?}");
        assert!(all.contains(&"我想想。".to_string()));
        assert!(all.contains(&"我想想怎么讲。".to_string()));
    }

    /// R1g/P4 + R1j/P0-a: the cue's start policy is one function. `radio`
    /// (default) may talk over a SONG only — the voice channel always belongs
    /// to the answer, so a live voice play blocks the cue in both modes.
    #[test]
    fn the_cue_never_starts_over_voice() {
        use crate::interaction::Content;
        let both = Content {
            voice: true,
            media: true,
        };
        assert!(cue_may_start(FillerMode::Radio, Content::NONE));
        assert!(
            cue_may_start(FillerMode::Radio, Content::media()),
            "radio talk over music"
        );
        assert!(
            !cue_may_start(FillerMode::Radio, Content::voice()),
            "voice is the answer's, always"
        );
        assert!(
            !cue_may_start(FillerMode::Radio, both),
            "a live answer owns the channel"
        );
        assert!(cue_may_start(FillerMode::Interrupt, Content::NONE));
        assert!(!cue_may_start(FillerMode::Interrupt, Content::media()));
        assert!(!cue_may_start(FillerMode::Interrupt, Content::voice()));
    }

    #[test]
    fn the_delay_is_bounded_and_phrases_are_validated() {
        let defaults = cfg();
        assert_eq!(defaults.delay_ms, 300, "the cue is an ack before the work");
        assert_eq!(
            defaults.tool_delay_ms, 600,
            "RUI's beat after a tool decision"
        );
        assert!(defaults.validate().is_ok());

        let mut short = cfg();
        short.delay_ms = 199;
        assert!(short.validate().is_err(), "below 200 ms must be rejected");
        let mut long = cfg();
        long.delay_ms = 10_001;
        assert!(long.validate().is_err(), "above 10 s must be rejected");

        let mut too_many = cfg();
        too_many.thinking = vec![
            "一".into(),
            "二".into(),
            "三".into(),
            "四".into(),
            "五".into(),
        ];
        assert!(
            too_many.validate().is_err(),
            "more than 4 phrases must be rejected"
        );

        let mut dup = cfg();
        dup.search = vec!["我查一下。".into(), "我查一下。".into()];
        assert!(
            dup.validate().is_err(),
            "duplicate phrases must be rejected"
        );

        let mut long_phrase = cfg();
        long_phrase.device = vec!["一".repeat(41)];
        assert!(
            long_phrase.validate().is_err(),
            "phrases are capped at 40 chars"
        );

        let mut blank = cfg();
        blank.story = vec!["   ".into()];
        assert!(blank.validate().is_err(), "a blank phrase is not a phrase");

        // All empty is legal: it is how every scene gets turned off.
        let mut off = cfg();
        off.thinking.clear();
        off.search.clear();
        off.device.clear();
        off.story.clear();
        assert!(
            off.validate().is_ok(),
            "all scenes off is a valid configuration"
        );
        assert!(off.pick("现在几点", 1).is_none());
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;

    fn rec(
        channel: &str,
        stage: &str,
        started_ms: u64,
        expected_ms: u64,
        end_sent: Option<u64>,
    ) -> OutputRecord {
        OutputRecord {
            answer_text: String::new(),
            request: None,
            task_id: Some("T1".into()),
            channel: channel.into(),
            kind: "answer".into(),
            content_ref: "tts:test".into(),
            stage: stage.into(),
            started_ms,
            stage_ms: end_sent.unwrap_or(started_ms),
            end_sent_ms: end_sent,
            expected_ms,
            synth_ms: 0,
        }
    }

    /// The 2026-11-13 field regression: a 19.3 s news program was stopped
    /// because `play_end` arrived while the device still had the tail queued.
    /// Start + duration + bound is the earliest honest deadline.
    /// R2 (2026-11-13): the spoken cap is BOTH limits at once — the sentence
    /// count and the character count — and it never re-opens once spent.
    #[test]
    fn the_spoken_cap_counts_sentences_and_characters_together() {
        let mut cap = SpokenCap::new(crate::agent::prompt::SpokenBudget {
            max_sentences: 2,
            max_chars: 30,
        });
        assert_eq!(cap.admit("第一句。").as_deref(), Some("第一句。"));
        // Fits the sentence count, not the character count (4 + 30 > 30).
        assert!(cap
            .admit("这一句故意写得很长很长很长很长，就是为了超过剩下的二十六个字。")
            .is_none());
        assert!(cap.exhausted, "a refusal spends the cap");
        assert!(cap.admit("短。").is_none(), "a spent cap never re-opens");
    }

    /// A first sentence longer than the whole cap is still SAID (a silent
    /// answer is worse), cut at a clause boundary rather than mid-phrase.
    #[test]
    fn a_first_sentence_longer_than_the_cap_is_cut_at_a_clause() {
        let mut cap = SpokenCap::new(crate::agent::prompt::SpokenBudget {
            max_sentences: 2,
            max_chars: 20,
        });
        let long = "前面这段话没有句号只有逗号，一直在说，后面还有";
        let said = cap.admit(long).expect("the first thing must be said");
        assert!(said.chars().count() <= 20, "{said}");
        assert!(said.ends_with('，'), "cut at a clause boundary: {said}");
        assert!(cap.exhausted);
    }

    /// The word lists are retired (2026-11-14): the model chooses the shape.
    /// What survives of the old tests is the BOOKMARK rule, which no longer
    /// reads the transcript at all — it reads the program's own size.
    #[test]
    fn a_program_is_content_by_its_size_not_by_keywords() {
        // A plain two-sentence confirmation: nothing left worth coming back to.
        let reply = Program {
            text: "已开始播放《Mojito》。".into(),
            short_chars: 120,
            complete: true,
            ..Program::default()
        };
        assert!(
            !VoicePipeline::resumable_program(&reply),
            "a music confirmation is not a program, keywords or not"
        );
        // The 2026-11-14 device case: ASR dropped 「讲」, so no keyword could
        // ever have matched. Size decides: a real story is worth a bookmark
        // whatever the transcript looked like.
        let story = Program {
            text: "很久很久以前有一座山。山下住着一位老人。".repeat(7),
            short_chars: 120,
            complete: true,
            ..Program::default()
        };
        assert!(VoicePipeline::resumable_program(&story));
        // Exactly one short answer's worth is still an ordinary reply.
        let edge = Program {
            text: "字".repeat(120),
            short_chars: 120,
            complete: true,
            ..Program::default()
        };
        assert!(!VoicePipeline::resumable_program(&edge));
        // Still being written: no size yet, so it is not dismissed. A story
        // paused two seconds in is the case 「继续」 exists for.
        let writing = Program {
            text: "第1段故事，讲的是山谷里的风。".into(),
            short_chars: 120,
            complete: false,
            ..Program::default()
        };
        assert!(VoicePipeline::resumable_program(&writing));
        // An explicit resumable (fault restore, fixtures) always resumes.
        let kept = Program {
            resumable: true,
            ..Program::default()
        };
        assert!(VoicePipeline::resumable_program(&kept));
    }

    /// A continuation reads a PARAGRAPH, not two sentences: the cap comes
    /// from the program's snapshot of the backstop, not from the short
    /// budget (device 2026-11-14: every 「继续」 gave 2 sentences and asked
    /// for the next 「继续」).
    #[test]
    fn a_continuation_reads_the_snapshotted_paragraph_not_two_sentences() {
        let settings = SpokenDeliverySettings::default();
        let story = Program {
            cont: Some(settings.backstop_budget()),
            ..Program::default()
        };
        let mut cap = VoicePipeline::continuation_cap(&story, &settings);
        let mut said = 0;
        for seg in ["第一句。", "第二句。", "第三句。", "第四句。"] {
            if cap.admit(seg).is_some() {
                said += 1;
            }
        }
        assert_eq!(said, 4, "a continuation must not stop at the short cap");

        // A program from before snapshots existed falls back to the live
        // backstop, which is still far wider than the short budget.
        let legacy = Program::default();
        let mut cap = VoicePipeline::continuation_cap(&legacy, &settings);
        assert!(
            cap.admit("一。").is_some()
                && cap.admit("二。").is_some()
                && cap.admit("三。").is_some()
        );
    }

    /// The resume lead labels the FIRST continuation only.
    #[test]
    fn the_resume_lead_is_said_once_per_program() {
        let mut p = Program {
            system_cut: true,
            ..Program::default()
        };
        assert_eq!(
            VoicePipeline::take_resume_lead(&mut p, "接着刚才的话题。").as_deref(),
            Some("接着刚才的话题。")
        );
        assert!(
            VoicePipeline::take_resume_lead(&mut p, "接着刚才的话题。").is_none(),
            "the second 「继续」 must not repeat the lead"
        );
        // A person's pause is never labelled.
        let mut paused = Program {
            system_cut: false,
            ..Program::default()
        };
        assert!(VoicePipeline::take_resume_lead(&mut paused, "接着刚才的话题。").is_none());
        // Empty lead = nothing, and it does not consume the once-flag.
        let mut fresh = Program {
            system_cut: true,
            ..Program::default()
        };
        assert!(VoicePipeline::take_resume_lead(&mut fresh, "").is_none());
    }

    /// The backstop exists to catch a runaway, so it must sit far above the
    /// soft targets — and validate() enforces that relation.
    #[test]
    fn the_backstop_is_far_above_the_soft_targets() {
        let cfg = SpokenDeliverySettings::default();
        cfg.validate().expect("defaults must validate");
        assert!(cfg.backstop_max_sentences > cfg.detail_max_sentences);
        assert!(cfg.backstop_max_chars > cfg.detail_max_chars);
        let bad = SpokenDeliverySettings {
            backstop_max_sentences: 6,
            backstop_max_chars: 300,
            ..SpokenDeliverySettings::default()
        };
        assert!(
            bad.validate().is_err(),
            "a backstop tighter than a soft target is a misconfiguration"
        );
    }

    #[test]
    fn a_still_draining_program_is_not_a_stall() {
        let r = rec("voice", "streaming", 1_000, 20_000, Some(4_000));
        assert!(!VoicePipeline::play_stall_due(
            &r,
            4_000 + PLAY_STALL_MS + 1
        ));
        assert!(!VoicePipeline::play_stall_due(&r, 1_000 + 20_000));
        assert!(VoicePipeline::play_stall_due(
            &r,
            1_000 + 20_000 + PLAY_STALL_MS
        ));
    }

    #[test]
    fn an_unknown_duration_falls_back_to_the_post_end_bound() {
        let r = rec("voice", "streaming", 0, 0, Some(0));
        assert!(!VoicePipeline::play_stall_due(&r, PLAY_STALL_MS - 1));
        assert!(VoicePipeline::play_stall_due(&r, PLAY_STALL_MS));
    }

    #[test]
    fn terminal_media_and_unclosed_plays_are_never_stalls() {
        assert!(!VoicePipeline::play_stall_due(
            &rec("voice", "failed", 0, 1, Some(0)),
            1_000_000
        ));
        assert!(!VoicePipeline::play_stall_due(
            &rec("media", "streaming", 0, 1, Some(0)),
            1_000_000
        ));
        assert!(!VoicePipeline::play_stall_due(
            &rec("voice", "streaming", 0, 1, None),
            1_000_000
        ));
    }
}

#[cfg(test)]
mod follow_window_tests {
    use super::*;

    #[test]
    fn d1_old_request_end_must_not_finish_same_generation_new_request() {
        let mut st = DeviceVoiceState::default();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy::default(),
                CreditGrant::Full,
                1_000,
            )
            .unwrap();
        let a = st
            .begin_window_request(0, st.participation.identity(), false)
            .unwrap();
        let b = st
            .begin_window_request(0, st.participation.identity(), false)
            .unwrap();
        assert_eq!(a.generation, b.generation);
        assert_ne!(a.request_id, b.request_id);
        assert_eq!(a.kind, participation::RequestKind::First);
        assert_eq!(b.kind, participation::RequestKind::Followup);
        assert!(!st.finish_window_request(a, 3_000));
        assert!(
            st.participation.contains(b),
            "A must not clear B's running state"
        );
        assert!(
            !st.finish_window_request(a, 4_000),
            "duplicate finish is inert"
        );
        assert!(st.finish_window_request(b, 5_000));
        assert!(!st.participation.request_running());
    }

    #[test]
    fn old_epoch_cannot_mark_or_finish_a_new_window_request() {
        let mut st = DeviceVoiceState::default();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy::default(),
                CreditGrant::Full,
                1_000,
            )
            .unwrap();
        let old = st
            .begin_window_request(0, st.participation.identity(), false)
            .unwrap();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy {
                    enabled: false,
                    ..Default::default()
                },
                CreditGrant::Full,
                2_000,
            )
            .unwrap();
        st.stop_epoch = 2;
        assert!(st
            .begin_window_request(1, st.participation.identity(), false)
            .is_none());
        assert!(!st.participation.request_started());
        assert!(!st.participation.request_running());
        let current = st
            .begin_window_request(2, st.participation.identity(), false)
            .unwrap();
        assert!(!st.finish_window_request(old, 3_000));
        assert!(st.participation.contains(current));
        assert!(st.finish_window_request(current, 4_000));
        assert!(!st.participation.request_running());
        assert!(st.participation.follow_window_blocked);
    }

    #[test]
    fn follow_window_defaults_and_validation_match_old_snapshots() {
        let old: VoiceSettings = serde_json::from_value(json!({})).unwrap();
        // 2026-11-15 (user): an answer ENDS the conversation; a person must not
        // have to say 「退下吧」 to stop the room being answered. The window is
        // now opt-in, and the mechanism's own default stays "on" for callers
        // that build a policy directly (see `FollowWindowPolicy::default`).
        assert!(
            !old.follow_window_enabled,
            "a fresh node closes after each answer"
        );
        assert!(FollowWindowPolicy::default().enabled);
        assert_eq!(old.follow_window_ms, 9_000);
        assert_eq!(old.context_window_ms, 240_000);
        assert_eq!(old.followup_credits, None);
        VoicePipeline::validate_settings(&old).unwrap();
        for ms in [4_000, 61_000] {
            let mut invalid = old.clone();
            invalid.follow_window_ms = ms;
            assert!(VoicePipeline::validate_settings(&invalid).is_err());
        }
        for n in [6] {
            let mut invalid = old.clone();
            invalid.followup_credits = Some(n);
            assert!(VoicePipeline::validate_settings(&invalid).is_err());
        }
        for (ms, credits) in [(5_000, 0), (5_000, 1), (60_000, 5)] {
            let mut valid = old.clone();
            valid.follow_window_ms = ms;
            valid.followup_credits = Some(credits);
            VoicePipeline::validate_settings(&valid).unwrap();
        }
    }

    #[test]
    fn configured_timeout_controls_both_admission_checks() {
        let mut st = DeviceVoiceState::default();
        st.participation.follow_window.ms = 5_000;
        st.participation.session.open(10_000);
        st.participation.last_activity_ms = 10_000;
        st.participation.last_session_open_ms = 10_000;
        assert!(st.session_open(14_999));
        assert!(!st.session_open(15_000));
        assert!(st.session_covers(11_000, 12_000, 12_000));
        assert!(!st.session_covers(15_001, 16_000, 16_000));
        st.participation.follow_window.ms = 60_000;
        assert!(st.session_open(69_999));
        assert!(!st.session_open(70_000));
    }

    #[test]
    fn disabled_follow_window_keeps_first_question_but_closes_after_last_answer() {
        let mut st = DeviceVoiceState::default();
        st.participation.follow_window.enabled = false;
        st.participation.session.open(1_000);
        st.participation.last_activity_ms = 1_000;
        st.participation.last_session_open_ms = 1_000;
        st.participation.follow_window.credits = Some(2);
        st.participation.followup_credits = 2;
        // Wake ack finishes before the first question: it must stay admitted
        // and, being an ack, must not spend or extend anything.
        end_voice_play(&mut st, "P-ack", "ack", None, 1_500);
        assert!(st.session_open(1_501));
        assert_eq!(st.spend_followup_credit(), Some(1));
        let request = st
            .begin_window_request(st.stop_epoch, st.participation.identity(), false)
            .unwrap();
        st.participation.answer_started(request, "answer");
        assert_eq!(st.spend_followup_credit(), None);
        assert!(!st.session_covers(2_000, 2_500, 2_500));
        st.outputs
            .insert("answer".into(), voice_output("device_confirmed"));
        // A cue ended mid-request: no authority over the window.
        end_voice_play(&mut st, "P-cue", "ack", None, 3_000);
        assert!(st.session_open(49_000));
        st.finish_window_request(request, 49_000);
        assert!(st.session_open(49_001)); // Last audio has not drained.
        st.outputs.clear();
        st.participation.answer_ended(request, "answer", 50_000);
        assert!(!st.session_open(50_001));
        assert_eq!(st.participation.followup_credits, 0);
        // A later cue cannot turn the closed window into a follow-up window.
        end_voice_play(&mut st, "P-cue2", "ack", None, 51_000);
        assert!(!st.session_open(51_001));
    }

    #[test]
    fn disabled_follow_window_closes_a_request_without_audio() {
        let mut st = DeviceVoiceState::default();
        st.participation.follow_window.enabled = false;
        st.participation.session.open(1_000);
        let request = st
            .begin_window_request(st.stop_epoch, st.participation.identity(), false)
            .unwrap();
        st.finish_window_request(request, 2_000);
        assert!(!st.session_open(2_001));
        assert!(st.participation.follow_window_blocked);
    }

    fn voice_output(stage: &str) -> OutputRecord {
        OutputRecord {
            answer_text: String::new(),
            request: None,
            task_id: Some("T1".into()),
            channel: "voice".into(),
            kind: "answer".into(),
            content_ref: "tts:test".into(),
            stage: stage.into(),
            started_ms: 0,
            stage_ms: 0,
            end_sent_ms: None,
            expected_ms: 0,
            synth_ms: 0,
        }
    }

    /// The 2026-11-13 production bug: a 49 s news answer. The wake opened the
    /// window at t=0; at t=49 s the answer is still playing. The old rule
    /// (30 s from the wake) had already closed it, so the user's reply was
    /// `mic_ignored_unwoken`. The window is open while the speaker is live.
    #[test]
    fn a_live_voice_play_keeps_the_window_open_past_the_wake_timeout() {
        let mut st = DeviceVoiceState::default();
        st.participation.session.open(0);
        st.participation.last_activity_ms = 0;
        let r = st
            .participation
            .begin_request(st.participation.identity(), 0, false)
            .unwrap();
        st.participation.answer_started(r, "P1");
        st.participation.finish_request(r, 0);
        let mut output = voice_output("device_confirmed");
        output.request = Some(r);
        st.outputs.insert("P1".into(), output);
        assert!(st.session_open(49_000), "the answer is still playing");
        assert!(
            st.session_open(DEFAULT_FOLLOW_WINDOW_MS + 19_000),
            "49 s of speech must not expire the window mid-answer"
        );
    }

    /// The follow-up budget: an activation grants DEFAULT_FOLLOWUP_CREDITS, ordinary
    /// follow-ups spend them, and the next transcript after the last credit is
    /// room speech (2026-11-14: the living room kept a window alive forever by
    /// talking through it).
    #[test]
    fn followup_credits_bound_the_chain() {
        let mut st = DeviceVoiceState::default();
        st.participation.follow_window.credits = Some(2);
        assert_eq!(
            st.participation.followup_credits, 0,
            "no activation, no follow-ups"
        );

        // An explicit activation is the only thing that refills to the cap.
        st.participation.follow_window.credits = Some(2);
        st.participation.followup_credits = 2;
        assert_eq!(st.spend_followup_credit(), Some(1));
        assert_eq!(st.spend_followup_credit(), Some(0));
        assert_eq!(st.spend_followup_credit(), None, "the third is room speech");
        assert_eq!(st.spend_followup_credit(), None, "and so is the fourth");

        // A stop owes exactly the next command, not a fresh allowance.
        st.participation.followup_credits = 1;
        assert_eq!(st.spend_followup_credit(), Some(0));
        assert_eq!(st.spend_followup_credit(), None);
    }

    /// Global reference: the window starts at the END of the last voice
    /// playback, not at the wake (a 49 s answer must not eat it), and closes
    /// exactly DEFAULT_FOLLOW_WINDOW_MS later.
    #[test]
    fn the_window_starts_at_the_last_voice_playback_end() {
        let mut st = DeviceVoiceState::default();
        st.participation.session.open(0);
        st.participation.last_activity_ms = 0;
        let r = st
            .participation
            .begin_request(st.participation.identity(), 0, false)
            .unwrap();
        st.participation.answer_started(r, "answer");
        st.participation.finish_request(r, 0);
        assert!(st.participation.answer_ended(r, "answer", 49_000));

        assert!(st.session_open(50_000));
        assert!(
            st.session_open(49_000 + DEFAULT_FOLLOW_WINDOW_MS - 1),
            "one ms inside the timeout"
        );
        assert!(
            !st.session_open(49_000 + DEFAULT_FOLLOW_WINDOW_MS),
            "the timeout is the boundary"
        );
    }

    /// The PlayFinished entrance for a voice play of one class, exactly as
    /// the role logic calls it. Contract v2 #4 (2026-11-16): `kind` decides.
    fn end_voice_play(
        st: &mut DeviceVoiceState,
        play_id: &str,
        kind: &str,
        request: Option<RequestTicket>,
        at: u64,
    ) {
        let mut rec = voice_output("device_confirmed");
        rec.kind = kind.into();
        rec.request = request;
        st.note_voice_play_end(play_id, &rec, at);
    }

    /// Unbound voice ends have no session or tail authority.
    #[test]
    fn unbound_voice_end_never_reopens_or_moves_the_reference() {
        let mut st = DeviceVoiceState::default(); // Idle, as after a sweep
        end_voice_play(&mut st, "P1", "answer", None, 10_000);
        assert_eq!(st.participation.session.state, SessionState::Idle);
        assert!(!st.session_open(10_000 + DEFAULT_FOLLOW_WINDOW_MS - 1));
        assert!(!st.session_open(10_000 + DEFAULT_FOLLOW_WINDOW_MS));
    }

    /// 退下 = 清零: the retreat ack is a voice play too, and its end must not
    /// reopen the session. The next wake clears the block.
    #[test]
    fn a_retreat_ack_does_not_reopen_the_window() {
        let mut st = DeviceVoiceState::default();
        st.participation.session.close(1_000);
        st.participation.follow_window_blocked = true;
        end_voice_play(&mut st, "P-retreat", "ack", None, 2_000);
        assert_eq!(st.participation.session.state, SessionState::Idle);
        assert!(!st.session_open(2_001));

        // The next wake clears the block and opens normally.
        st.participation.follow_window_blocked = false;
        st.participation.session.open(3_000);
        st.participation.last_activity_ms = 3_000;
        assert!(st.session_open(3_001));
    }

    /// Release 33: the window-close cue announced the boundary and its own
    /// tail re-opened the window, so dot2-e said 「拜拜啦」every 21 s forever
    /// (live evidence 2026-11-14 23:48–23:49). Contract v2 #4 retires the
    /// per-play patch: the cue is an `ack`, and an ack's end has no
    /// authority at all — the same gate covers every cue added later.
    #[test]
    fn the_close_cue_does_not_reopen_the_window_it_announces() {
        let mut st = DeviceVoiceState::default();
        st.participation.session.close(1_000);
        end_voice_play(&mut st, "P-close", "ack", None, 5_000);
        assert_eq!(st.participation.session.state, SessionState::Idle);
        assert!(
            !st.session_open(5_001),
            "the cue must not re-open the window"
        );
        assert!(!st.session_open(5_000 + DEFAULT_FOLLOW_WINDOW_MS));

        // Nor may a later end of any other class.
        end_voice_play(&mut st, "P-answer", "answer", None, 30_000);
        assert_eq!(st.participation.session.state, SessionState::Idle);
        assert!(!st.session_open(30_001));
    }

    /// A wake racing with the close cue keeps its own window: the cue's end
    /// is simply a cue end, so it cannot close or move the wake's window.
    #[test]
    fn a_wake_racing_with_the_close_cue_keeps_its_window() {
        let mut st = DeviceVoiceState::default();
        st.participation.session.close(1_000);
        // The wake opens the window, then the cue's own play ends.
        st.participation.session.open(2_000);
        st.participation.last_activity_ms = 2_000;
        end_voice_play(&mut st, "P-ack", "ack", None, 2_500);
        assert!(st.session_open(2_501));
        end_voice_play(&mut st, "P-close", "ack", None, 3_000);
        assert!(
            st.session_open(3_001),
            "the cue's end must not close the wake's window"
        );
    }

    /// Contract v2 #4 (2026-11-16), the rule itself in one place: of the five
    /// voice classes a device can finish, ONLY an answer bound to a request
    /// moves the tail. ack (wake ack / close announcement / retreat ack),
    /// filler, prompt (truncation notice) and probe end as facts.
    #[test]
    fn only_an_answer_end_moves_the_follow_window() {
        for kind in ["ack", "filler", "prompt", "probe"] {
            let mut st = DeviceVoiceState::default();
            st.participation.session.open(1_000);
            st.participation.last_activity_ms = 1_000;
            let r = st
                .participation
                .begin_request(st.participation.identity(), 1_000, false)
                .unwrap();
            st.participation.answer_started(r, "P");
            // The answer's request is still open, the audio "of this class"
            // ends: nothing may move.
            end_voice_play(&mut st, "P", kind, Some(r), 5_000);
            assert_eq!(
                st.participation.last_activity_ms, 1_000,
                "{kind} moved last_activity_ms"
            );
            assert_eq!(
                st.last_voice_end_ms, 0,
                "{kind} was treated as a continuation of an answer"
            );
        }

        let mut st = DeviceVoiceState::default();
        st.participation.session.open(1_000);
        st.participation.last_activity_ms = 1_000;
        let r = st
            .participation
            .begin_request(st.participation.identity(), 1_000, false)
            .unwrap();
        st.participation.answer_started(r, "P");
        st.participation.finish_request(r, 5_000);
        end_voice_play(&mut st, "P", "answer", Some(r), 5_000);
        assert_eq!(st.last_voice_end_ms, 5_000, "an answer is the reference");
        assert!(st.session_open(5_000 + DEFAULT_FOLLOW_WINDOW_MS - 1));
        assert!(!st.session_open(5_000 + DEFAULT_FOLLOW_WINDOW_MS));
    }

    /// The reference has two jobs; a cue must not fake the second one. An
    /// ack that ends 100 ms before a new answer is not "the same answer
    /// continuing", so the answer still arms its barge-in protection period.
    /// Before contract v2 #4 every voice end moved the reference, so this
    /// protection was silently skipped after every wake ack.
    #[test]
    fn a_cue_end_does_not_make_the_next_answer_a_continuation() {
        for (kind, arms_protection) in [("ack", true), ("answer", false)] {
            let mut st = DeviceVoiceState::default();
            end_voice_play(&mut st, "P", kind, None, 1_500);
            VoicePipeline::note_voice_play_submitted_locked(&mut st, 1_600, 800);
            assert_eq!(
                st.barge_protect_until_ms,
                if arms_protection { 2_400 } else { 0 },
                "kind={kind}: protection arming is wrong"
            );
        }
    }

    fn bookmark(text: &str, cursor: usize, system_cut: bool) -> Program {
        Program {
            text: text.into(),
            cursor,
            complete: true,
            created_ms: 0,
            resumable: true,
            system_cut,
            ..Program::default()
        }
    }

    /// The 2026-11-14 device report: a budget-cut ANSWER held the amber ring
    /// through a session exit — nobody paused anything, yet the ring said
    /// "on hold". Amber is the person's pause; a system cut keeps the ring on
    /// the follow window (or idle).
    #[test]
    fn a_system_cut_bookmark_lights_no_amber_a_person_pause_does() {
        let mut st = DeviceVoiceState::default();
        st.paused_program = Some(bookmark("前半句。后半句还很长。", 12, true));
        assert_ne!(
            st.assistant_state(1_000, HubLive::default(), false),
            "paused"
        );

        // The same bookmark after an explicit 「暂停」: amber, inside the TTL.
        let mut st = DeviceVoiceState::default();
        st.paused_program = Some(bookmark("第1段故事。第2段故事。", 0, false));
        assert_eq!(
            st.assistant_state(1_000, HubLive::default(), false),
            "paused"
        );
        // And it expires: the ring goes dark after the TTL even for a pause.
        assert_ne!(
            st.assistant_state(PROGRAM_TTL_MS + 1, HubLive::default(), false),
            "paused"
        );
    }

    /// When the follow window closes by timeout, a cut answer's offer dies
    /// with the session — "it left" must not leave something behind. A
    /// person's pause survives the window (walk away from a story, come back
    /// with 「继续」).
    #[test]
    fn the_window_close_drops_a_system_cut_and_keeps_a_pause() {
        let mut cut = DeviceVoiceState::default();
        cut.paused_program = Some(bookmark("开头。剩下的都留在这里。", 9, true));
        let left = cut.drop_system_cut();
        assert_eq!(left, Some("剩下的都留在这里。".len()));
        assert!(cut.paused_program.is_none());

        let mut paused = DeviceVoiceState::default();
        paused.paused_program = Some(bookmark("第1段故事。第2段故事。", 0, false));
        assert_eq!(paused.drop_system_cut(), None, "a person's pause is kept");
        assert!(paused.paused_program.is_some());

        // Nothing held: dropping is a no-op, not an error.
        let mut empty = DeviceVoiceState::default();
        assert_eq!(empty.drop_system_cut(), None);
    }

    /// A resumed CUT answer announces itself in its first sentence; a
    /// person's pause does not (the amber ring already said "on hold").
    /// Asserted on the pipeline inputs: the lead is a settings string, the
    /// decision rides solely on `system_cut`.
    #[test]
    fn the_resume_lead_applies_to_system_cuts_only() {
        let cut = bookmark("前半句。后半句。", 12, true);
        assert!(cut.system_cut);
        let paused = bookmark("第1段故事。第2段故事。", 0, false);
        assert!(!paused.system_cut);
        // The default lead is a valid, non-empty spoken phrase.
        let lead = default_resume_lead();
        assert!(!lead.trim().is_empty());
        let cfg = SpokenDeliverySettings::default();
        assert_eq!(cfg.resume_lead, lead);
        cfg.validate().expect("defaults must validate");
    }
}

#[cfg(test)]
pub(in crate::voice) mod phase3_event_pump_tests {
    use super::*;
    use base64::Engine;
    use std::time::Duration;

    pub(in crate::voice) fn test_pipeline(hub: Arc<DeviceHub>) -> Arc<VoicePipeline> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pipe = VoicePipeline::new(
            hub,
            store.clone(),
            Arc::new(TaskRegistry::new(store)),
            Arc::new(|| Err("isolated: no agent".into())),
            Arc::new(|| Err("isolated: no TTS".into())),
            Arc::new(|| Err("isolated: no ASR".into())),
            std::env::temp_dir().join(id::next("hm-phase3")),
        );
        crate::lock_or_recover(&pipe.settings, "voice.settings").follow_window_enabled = true;
        pipe.project_follow_window_policy();
        pipe
    }

    fn audio_chunk_frame(device: &str, seq: u64, samples: usize) -> crate::gateway::GatewayFrame {
        let pcm_b64 = base64::engine::general_purpose::STANDARD.encode(vec![0u8; samples * 2]);
        serde_json::from_value(json!({
            "type": "audio_chunk",
            "device": device,
            "ts": 1000 + seq,
            "seq": seq,
            "pcm_b64": pcm_b64,
            "format": "s16le",
            "sample_rate": 16000,
            "channels": 1,
            "layout": "mono"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn audio_chunk_flood_does_not_drop_control_events() {
        let hub = DeviceHub::new();
        hub.set_generation_for_test(1);
        let mut ctrl_rx = hub.subscribe_control();
        let mut audio_rx = hub.subscribe_audio();

        // Flood 500 AudioChunk frames (> 256 audio channel capacity) with
        // interleaved control events (Keyword, Wake, PlayFinished).
        for seq in 1..=500u64 {
            hub.handle_frame_for_test(1, audio_chunk_frame("dot2-d", seq, 160));
            if seq == 100 {
                hub.handle_frame_for_test(
                    1,
                    serde_json::from_value(json!({
                        "type": "keyword",
                        "device": "dot2-d",
                        "ts": 2000,
                        "seq": 9000,
                        "keyword": "你好高达"
                    }))
                    .unwrap(),
                );
            } else if seq == 250 {
                hub.handle_frame_for_test(
                    1,
                    serde_json::from_value(json!({
                        "type": "wake",
                        "device": "dot2-d",
                        "ts": 2100,
                        "seq": 9001
                    }))
                    .unwrap(),
                );
            } else if seq == 400 {
                hub.handle_frame_for_test(
                    1,
                    serde_json::from_value(json!({
                        "type": "play_finished",
                        "device": "dot2-d",
                        "task_id": "t1",
                        "play_id": "p1",
                        "played_ms": 500,
                        "ts": 2200
                    }))
                    .unwrap(),
                );
            }
        }

        // Audio channel must report Lagged because 500 > 256.
        assert!(matches!(
            audio_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        // Control channel must deliver all 3 control events with zero drops.
        assert!(matches!(
            ctrl_rx.try_recv().unwrap(),
            DeviceEvent::Keyword { .. }
        ));
        assert!(matches!(
            ctrl_rx.try_recv().unwrap(),
            DeviceEvent::Wake { .. }
        ));
        assert!(matches!(
            ctrl_rx.try_recv().unwrap(),
            DeviceEvent::PlayFinished { .. }
        ));

        // Also verify end-to-end through VoicePipeline::start().
        let pipe = test_pipeline(hub.clone());
        pipe.start();
        for seq in 501..=1000u64 {
            hub.handle_frame_for_test(1, audio_chunk_frame("dot2-d", seq, 160));
        }
        hub.handle_frame_for_test(
            1,
            serde_json::from_value(json!({
                "type": "wake",
                "device": "dot2-d",
                "ts": 3000,
                "seq": 9002
            }))
            .unwrap(),
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let open = crate::lock_or_recover(&pipe.states, "voice.states")
                .get("dot2-d")
                .is_some_and(|st| st.session_open(id::now_ms()));
            if open {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "wake was not processed after audio flood"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(pipe.control_lagged_total(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_silero_vad_does_not_block_control_events() {
        let hub = DeviceHub::new();
        hub.set_generation_for_test(1);
        let pipe = test_pipeline(hub.clone());

        // Install a Silero detector that sleeps 350 ms per 512-sample frame.
        let det = crate::silero::SileroDetector::for_test(|_| {
            std::thread::sleep(std::time::Duration::from_millis(350));
            Ok(0.1)
        });
        let proc = MicProcessor::new(pipe.live_vad_config()).with_silero(det);
        crate::lock_or_recover(&pipe.mic, "voice.mic")
            .insert("dot2-d".into(), Arc::new(Mutex::new(proc)));
        pipe.start();

        // Send a 1280-sample (80 ms) AudioChunk so the audio worker enters slow Silero inference.
        hub.handle_frame_for_test(1, audio_chunk_frame("dot2-d", 1, 1280));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Now send Wake on the control channel; it must be processed well before 350 ms.
        let start = std::time::Instant::now();
        hub.handle_frame_for_test(
            1,
            serde_json::from_value(json!({
                "type": "wake",
                "device": "dot2-d",
                "ts": 5000,
                "seq": 100
            }))
            .unwrap(),
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(150);
        loop {
            let open = crate::lock_or_recover(&pipe.states, "voice.states")
                .get("dot2-d")
                .is_some_and(|st| st.session_open(id::now_ms()));
            if open {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "control event stalled behind slow Silero VAD ({:?})",
                start.elapsed()
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn lock_poison_recovery_keeps_pump_alive() {
        let hub = DeviceHub::new();
        hub.set_generation_for_test(1);
        let pipe = test_pipeline(hub.clone());
        pipe.start();

        let before = crate::lock_poison_total();
        let pipe_clone = pipe.clone();
        let _ = std::thread::spawn(move || {
            let _g = pipe_clone.states.lock().unwrap();
            panic!("simulated panic while holding voice.states");
        })
        .join();
        hub.poison_state_for_test();

        // Pump and hub must recover from poisoned mutexes and keep serving.
        let _snap = hub.snapshot();
        hub.handle_frame_for_test(
            1,
            serde_json::from_value(json!({
                "type": "wake",
                "device": "dot2-d",
                "ts": 6000,
                "seq": 200
            }))
            .unwrap(),
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let open = crate::lock_or_recover(&pipe.states, "voice.states")
                .get("dot2-d")
                .is_some_and(|st| st.session_open(id::now_ms()));
            if open {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "pump died after mutex poison"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(crate::lock_poison_total() >= before + 2);
    }

    #[test]
    fn barge_state_machine_only_takes_head_in_feeding() {
        let mut b = Barge::Idle;
        assert!(b.take_head().is_none(), "Idle cannot yield head");

        b.stop(
            1000,
            300,
            vec![10, 20, 30],
            16_000,
            crate::mic::CaptureProvenance::legacy(3),
            800,
        );
        assert!(
            b.take_head().is_none(),
            "Stopped must not yield head before guard expires"
        );

        // Advance past guard_until_ms -> Feeding
        b.expire(300);
        assert!(matches!(b, Barge::Feeding { .. }));
        let taken = b.take_head();
        assert_eq!(
            taken.as_ref().map(|(h, r, _)| (h.as_slice(), *r)),
            Some((&[10, 20, 30][..], 16_000))
        );
        assert!(
            b.take_head().is_none(),
            "head is taken at most once in Feeding"
        );

        // Expired episode drops stale head before transition to Feeding
        let mut expired = Barge::Stopped {
            until_ms: 1000,
            guard_until_ms: 300,
            head: vec![99, 100],
            head_sample_rate: 16_000,
            head_provenance: crate::mic::CaptureProvenance::legacy(2),
            head_until_ms: 500,
        };
        expired.expire(600);
        assert!(
            expired.take_head().is_none(),
            "stale head past head_until_ms must be dropped"
        );
    }

    #[tokio::test]
    async fn concurrent_turns_cannot_both_submit_play_begin() {
        let hub = DeviceHub::new();
        hub.set_generation_for_test(1);
        let pipe = test_pipeline(hub.clone());
        let dev = "dot2-d";
        let now = id::now_ms();
        let (epoch, req1, turn1, turn2) = {
            let mut states = crate::lock_or_recover(&pipe.states, "voice.states");
            let st = states.entry(dev.into()).or_default();
            let id1 = st
                .participation
                .activate(
                    st.participation.identity(),
                    FollowWindowPolicy::default(),
                    CreditGrant::Full,
                    now,
                )
                .unwrap();
            let r1 = st.participation.begin_request(id1, 0, false).unwrap();
            let t1 = st.claim_turn();
            let t2 = st.claim_turn();
            (st.stop_epoch, r1, t1, t2)
        };
        assert!(turn2 > turn1);
        let mut stream1 = None;
        // Turn 1 was superseded by Turn 2, so speak_stream_segment for turn1 must refuse to submit play_begin.
        let ok1 = pipe
            .speak_stream_segment(
                dev,
                "第一句话。",
                &mut stream1,
                &None,
                epoch,
                turn1,
                Some(req1),
            )
            .await;
        assert!(!ok1, "superseded turn1 must not submit play_begin");
        assert!(stream1.is_none());

        // Turn 2 is the live turn; no two concurrent turns can both hold live_turn == Some(my_turn_id).
        let live_now = crate::lock_or_recover(&pipe.states, "voice.states")
            .get(dev)
            .and_then(|st| st.live_turn);
        assert_eq!(live_now, Some(turn2));
    }

    #[tokio::test]
    async fn unified_retreat_words_and_kws_asr_dedup() {
        let hub = DeviceHub::new();
        let pipe = test_pipeline(hub.clone());
        let dev = "dot2-d";

        // 1. ReflexConfig retreat word is compiled into effective_retreat_words (single source of truth).
        let cfg_svc = crate::config::ConfigService::new(pipe.store.clone());
        let mut cfg = cfg_svc.reflex().unwrap();
        cfg.words.push(crate::config::ReflexWord {
            text: "告辞了".into(),
            tokens: "g ào c í l e".into(),
            intent: crate::config::ReflexIntent::Retreat,
            enabled: true,
        });
        cfg_svc.save_reflex(cfg.clone(), "test").unwrap();
        let (kws_retreat, _) = crate::config::reflex_intent_map(&cfg);
        let (_, asr_retreat) = pipe.effective_retreat_policy();
        assert!(kws_retreat.contains(&"告辞了".to_string()));
        assert!(asr_retreat.contains(&"告辞了".to_string()));

        // 2. KWS wake + delayed ASR wake transcript within dedup window does not double-trigger.
        {
            let mut states = crate::lock_or_recover(&pipe.states, "voice.states");
            let st = states.entry(dev.into()).or_default();
            st.last_reflex = Some((interaction::KeywordIntent::Wake, id::now_ms()));
        }
        assert!(
            !pipe.admit_asr_wake(dev, "你好高达", "你好高达").await,
            "ASR fallback wake within REFLEX_DEDUPE_MS of KWS wake must be deduped"
        );
        assert!(pipe.recent(10).iter().any(|e| e["kind"] == "intent_deduped"
            && e["intent"] == "wake"
            && e["via"] == "asr_wake"));

        // 3. KWS retreat + delayed ASR retreat transcript within dedup window does not double-execute.
        {
            let mut states = crate::lock_or_recover(&pipe.states, "voice.states");
            let st = states.entry(dev.into()).or_default();
            st.last_reflex = Some((interaction::KeywordIntent::Retreat, id::now_ms()));
        }
        assert!(
            pipe.reflex_deduped(dev, interaction::KeywordIntent::Retreat, "followup"),
            "ASR retreat within REFLEX_DEDUPE_MS of KWS retreat must be deduped"
        );
    }

    #[tokio::test]
    async fn critical_control_events_survive_control_flood_and_broadcast_lag() {
        let hub = DeviceHub::new();
        let pipe = test_pipeline(hub.clone());
        hub.set_generation_for_test(1);
        hub.handle_frame_for_test(
            1,
            crate::gateway::GatewayFrame::DeviceStatus {
                northbound: None,
                device: "dot2-d".into(),
                online: true,
                reason: "online".into(),
                descriptor: None,
            },
        );

        // Send a critical Wake event BEFORE flooding >4,096 non-critical control events.
        hub.handle_frame_for_test(
            1,
            crate::gateway::GatewayFrame::Wake {
                origin: crate::gateway::InputOrigin::Legacy,
                device: "dot2-d".into(),
                ts: id::now_ms(),
                seq: 1,
            },
        );
        for i in 0..4_200u64 {
            let frame: crate::gateway::GatewayFrame = serde_json::from_value(json!({
                "type": "mic_report",
                "device": "dot2-d",
                "active": true,
                "detail": format!("flood-{i}")
            }))
            .unwrap();
            hub.handle_frame_for_test(1, frame);
        }

        // Start the pump AFTER the flood so broadcast receiver sees Lagged(n),
        // and verify Wake is still recovered via the critical fallback path.
        pipe.start();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut saw_wake = false;
        while tokio::time::Instant::now() < deadline {
            let listening = crate::lock_or_recover(&pipe.states, "voice.states")
                .get("dot2-d")
                .is_some_and(|st| st.participation.session.state == SessionState::Listening);
            if listening {
                saw_wake = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saw_wake,
            "critical Wake event must survive >4096 control flood"
        );
        assert!(
            pipe.control_lagged_total() > 0,
            "flood must have caused broadcast lag"
        );
    }

    #[tokio::test]
    async fn pump_metrics_reports_nonzero_queue_age_when_queued() {
        let hub = DeviceHub::new();
        let pipe = test_pipeline(hub.clone());
        hub.set_generation_for_test(1);
        let frame: crate::gateway::GatewayFrame = serde_json::from_value(json!({
            "type": "mic_report",
            "device": "dot2-d",
            "active": true,
            "detail": "queued"
        }))
        .unwrap();
        hub.handle_frame_for_test(1, frame);
        tokio::time::sleep(Duration::from_millis(25)).await;
        pipe.start();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut metrics = pipe.pump_metrics();
        while tokio::time::Instant::now() < deadline {
            metrics = pipe.pump_metrics();
            if metrics["max_pump_queue_age_ms"].as_u64().unwrap_or(0) >= 15 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(
            metrics["max_control_queue_age_ms"].as_u64().unwrap_or(0) >= 15,
            "expected non-zero control queue age, got {metrics}"
        );
        assert!(
            metrics["max_pump_queue_age_ms"].as_u64().unwrap_or(0) >= 15,
            "expected non-zero pump queue age, got {metrics}"
        );
    }

    #[tokio::test]
    async fn late_old_turn_audio_invalidated_on_wake_and_retreat() {
        let hub = DeviceHub::new();
        let pipe = test_pipeline(hub.clone());
        hub.set_generation_for_test(1);
        pipe.on_device_online("dot2-d", &[]);
        let cur_id = crate::lock_or_recover(&pipe.states, "voice.states")["dot2-d"]
            .participation
            .identity();
        let _id1 = pipe
            .activate_session_at("dot2-d", CreditGrant::Full, cur_id, "test")
            .unwrap();
        crate::lock_or_recover(&pipe.states, "voice.states")
            .get_mut("dot2-d")
            .unwrap()
            .live_turn = Some(101);
        assert_eq!(
            crate::lock_or_recover(&pipe.states, "voice.states")["dot2-d"].live_turn,
            Some(101)
        );

        // Retreat/exit word immediately invalidates live_turn before any new turn starts.
        pipe.handle(crate::gateway::DeviceEvent::Utterance {
            device: "dot2-d".into(),
            ts: id::now_ms(),
            seq: 2,
            source: "test".into(),
            text: "退下".into(),
            provenance: crate::gateway::InputProvenance {
                origin: crate::gateway::InputOrigin::Legacy,
                transport: None,
            },
        })
        .await;
        assert_eq!(
            crate::lock_or_recover(&pipe.states, "voice.states")["dot2-d"].live_turn,
            None,
            "retreat must immediately invalidate in-flight live_turn"
        );

        // Start a new turn and verify Wake also immediately invalidates live_turn.
        let cur_id2 = crate::lock_or_recover(&pipe.states, "voice.states")["dot2-d"]
            .participation
            .identity();
        let _id2 = pipe
            .activate_session_at("dot2-d", CreditGrant::Full, cur_id2, "test2")
            .unwrap();
        crate::lock_or_recover(&pipe.states, "voice.states")
            .get_mut("dot2-d")
            .unwrap()
            .live_turn = Some(102);
        pipe.handle(crate::gateway::DeviceEvent::Wake {
            device: "dot2-d".into(),
            ts: id::now_ms(),
            seq: 3,
            provenance: crate::gateway::InputProvenance {
                origin: crate::gateway::InputOrigin::Legacy,
                transport: None,
            },
        })
        .await;
        assert_eq!(
            crate::lock_or_recover(&pipe.states, "voice.states")["dot2-d"].live_turn,
            None,
            "wake must immediately invalidate previous live_turn"
        );
    }

    #[tokio::test]
    async fn mute_hold_wifi_setup_tracks_device_and_clears_on_wifi_connected_without_unmute_conflict(
    ) {
        let hub = DeviceHub::new();
        let pipe = test_pipeline(hub);
        pipe.on_device_online("dot2-d", &["remote_mute".into(), "wifi".into()]);

        // Broad 113_hold* must NOT trigger provisioning.
        pipe.handle(crate::gateway::DeviceEvent::Button {
            device: "dot2-d".into(),
            ts: id::now_ms(),
            seq: 1,
            action: "113_hold:3200ms".into(),
        })
        .await;
        assert!(pipe.wifi_provisioning_devices().is_empty());

        // Exact mute_hold_wifi_setup tracks the device and logs prompt/notice.
        pipe.handle(crate::gateway::DeviceEvent::Button {
            device: "dot2-d".into(),
            ts: id::now_ms(),
            seq: 2,
            action: "mute_hold_wifi_setup".into(),
        })
        .await;
        assert_eq!(pipe.wifi_provisioning_devices(), vec!["dot2-d".to_string()]);
        assert!(pipe
            .recent(20)
            .iter()
            .any(|e| e["kind"] == "wifi_provisioning_prompt"));

        // Remote MuteSet unmute (MuteState muted=false, source=core_set) does NOT conflict with provisioning state.
        pipe.handle(crate::gateway::DeviceEvent::MuteState {
            device: "dot2-d".into(),
            ts: id::now_ms(),
            seq: 3,
            muted: false,
            source: "core_set".into(),
            task_id: Some("M-1".into()),
        })
        .await;
        assert_eq!(pipe.wifi_provisioning_devices(), vec!["dot2-d".to_string()]);

        // WifiState connected auto-clears provisioning.
        pipe.handle(crate::gateway::DeviceEvent::WifiState {
            device: "dot2-d".into(),
            ssid: "Home".into(),
            state: "connected".into(),
            detail: None,
            ts: id::now_ms(),
        })
        .await;
        assert!(pipe.wifi_provisioning_devices().is_empty());
    }

    #[tokio::test]
    async fn asr_wake_blocked_by_sliding_window_and_mic_gain_offset_applied() {
        let pipe = test_pipeline(DeviceHub::new());
        let dev = "dot2-d";
        pipe.set_wake_arbitration(dev, WakeArbitration::Enforce, "test")
            .unwrap();
        let peer = crate::arbitration::Claim::new("dot2-e", id::now_ms() as i64 - 400, Some(-20.0));
        assert_eq!(pipe.arbiter.inbound(peer)["state"], "no_round");
        assert!(!pipe.admit_asr_wake(dev, "你好高达", "你好高达").await);
        assert!(pipe
            .recent(10)
            .iter()
            .any(|e| e["kind"] == "arbitration_lost" && e["via"] == "asr_wake_sliding_window"));
        let fleet = crate::fleet::FleetService::new(pipe.store.clone());
        fleet
            .upsert("dot2-e", "http://127.0.0.1:9", Some("tok"), false)
            .unwrap();
        let cfg = crate::config::ConfigService::new(pipe.store.clone());
        let prof = crate::config::UnitAcousticProfile {
            mic_gain_offset_db: 8.5,
            ..Default::default()
        };
        cfg.save_unit_acoustic_profile(dev, prof, "test").unwrap();
        let claim = pipe
            .wake_claim_for(dev, "你好高达", id::now_ms(), None, Some(-30.0))
            .unwrap();
        assert!((claim.rms_dbfs.unwrap() - (-21.5)).abs() < 0.01);
    }

    #[tokio::test]
    async fn p1_09_media_playback_clears_follow_open_edge_and_prevents_bogus_farewell() {
        let pipe = test_pipeline(DeviceHub::new());
        let dev = "dot2-d";
        let now = id::now_ms();
        {
            let mut states = pipe.states.lock().unwrap();
            let st = states.entry(dev.into()).or_default();
            st.participation
                .activate(
                    st.participation.identity(),
                    FollowWindowPolicy {
                        ms: 5_000,
                        ..Default::default()
                    },
                    CreditGrant::Full,
                    now.saturating_sub(20_000),
                )
                .unwrap();
            st.follow_open_last_tick = true;
            st.follow_close_pending_at = Some(now.saturating_sub(1_000));
            st.outputs.insert(
                "media-1".into(),
                OutputRecord {
                    answer_text: String::new(),
                    request: None,
                    task_id: None,
                    channel: "media".into(),
                    kind: "media".into(),
                    content_ref: "song".into(),
                    stage: "playing".into(),
                    started_ms: now.saturating_sub(5_000),
                    stage_ms: now.saturating_sub(5_000),
                    end_sent_ms: None,
                    expected_ms: 0,
                    synth_ms: 0,
                },
            );
        }
        // Sweep while media is live must clear `follow_open_last_tick` & `follow_close_pending_at`.
        pipe.follow_window_tone_sweep();
        {
            let mut states = pipe.states.lock().unwrap();
            let st = states.get_mut(dev).unwrap();
            assert!(!st.follow_open_last_tick);
            assert!(st.follow_close_pending_at.is_none());
            // Now music ends.
            st.outputs.remove("media-1");
        }
        // Subsequent sweep after music finishes must NOT emit a delayed bogus farewell.
        pipe.follow_window_tone_sweep();
        assert!(
            !pipe
                .recent(20)
                .iter()
                .any(|e| e["kind"] == "follow_window_close" && e["device"] == dev),
            "finishing music must never trigger a delayed bogus follow_window_close farewell"
        );
    }

    #[tokio::test]
    async fn p1_09_session_sweep_logs_program_cut_dropped_once_without_double_drop() {
        let pipe = test_pipeline(DeviceHub::new());
        let dev = "dot2-d";
        let now = id::now_ms();
        {
            let mut states = pipe.states.lock().unwrap();
            let st = states.entry(dev.into()).or_default();
            st.participation
                .activate(
                    st.participation.identity(),
                    FollowWindowPolicy {
                        ms: 5_000,
                        ..Default::default()
                    },
                    CreditGrant::Full,
                    now.saturating_sub(20_000),
                )
                .unwrap();
            st.follow_open_last_tick = true;
            st.paused_program = Some(Program {
                text: "这是一段被系统截断的长答案文本".into(),
                cursor: 0,
                complete: true,
                origin_task: None,
                created_ms: now.saturating_sub(15_000),
                resumable: true,
                system_cut: true,
                short_chars: 10,
                cont: None,
                lead_said: false,
            });
        }
        let closed = pipe.session_sweep_at(now);
        assert_eq!(closed, vec![dev.to_string()]);
        pipe.follow_window_tone_sweep();
        let cut_notes = pipe
            .recent(30)
            .into_iter()
            .filter(|e| e["kind"] == "program_cut_dropped" && e["device"] == dev)
            .count();
        assert_eq!(
            cut_notes, 1,
            "program_cut_dropped must be logged exactly once across session_sweep_at + follow_window_tone_sweep"
        );
    }

    #[tokio::test]
    async fn p1_04_failed_v2_reconciliation_resets_flags_for_retry() {
        let pipe = test_pipeline(DeviceHub::new());
        let dev = "dot2-d";
        let cfg = crate::config::ConfigService::new(pipe.store.clone());
        cfg.save_unit_acoustic_profile(
            dev,
            crate::config::UnitAcousticProfile {
                volume_cap: 0.75,
                ..Default::default()
            },
            "test",
        )
        .unwrap();
        pipe.set_mode_with_follow_window(
            dev,
            None,
            None,
            Some(vec!["你好小花".into()]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            FollowWindowPatch::default(),
            "test",
        )
        .unwrap();
        // Trigger on_device_online with v2 caps while hub has NO active outbound for `dev`,
        // so both acoustics and KWS dispatches fail transiently and must reset reconciled flags!
        pipe.on_device_online(
            dev,
            &[
                "config_apply_v2".into(),
                "config_block_acoustics".into(),
                "config_block_kws".into(),
            ],
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let (ac, kw) = {
                let states = pipe.states.lock().unwrap();
                let st = &states[dev];
                (st.acoustics_reconciled, st.kws_reconciled)
            };
            if !ac && !kw {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "failed v2 reconciliation must reset acoustics_reconciled ({ac}) and kws_reconciled ({kw})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
