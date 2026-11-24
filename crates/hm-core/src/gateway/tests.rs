use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// v1 mechanism (#8/#11): compatibility is interval intersection. The
    /// naive "peer version inside my range, checked on both sides" rule
    /// degenerates into "highest versions equal" — exactly the failure that
    /// would have blocked the version bump in the mixed activation window.
    #[test]
    fn protocol_intervals_intersect_not_mirror() {
        let core = (0u32, 1u32); // release B
        let firmware_old = (0u32, 0u32); // release A or the strict era
                                         // [0,1] vs [0,0]: the OLD era's core would refuse a v1 peer; the
                                         // intersection rule talks at 0.
        assert_eq!(protocol_operating_version(core, firmware_old), Some(0));
        assert_eq!(protocol_operating_version(firmware_old, core), Some(0));
        // Same-version pairs keep working, at that version.
        assert_eq!(protocol_operating_version((0, 0), (0, 0)), Some(0));
        assert_eq!(protocol_operating_version((0, 1), (0, 1)), Some(1));
        assert_eq!(protocol_operating_version((0, 1), (1, 1)), Some(1));
        // Disjoint intervals reject in BOTH directions.
        assert_eq!(protocol_operating_version((0, 1), (2, 3)), None);
        assert_eq!(protocol_operating_version((2, 3), (0, 1)), None);
        // An illegal interval (min > max) never intersects.
        assert_eq!(protocol_operating_version((1, 0), (0, 0)), None);
        assert_eq!(protocol_operating_version((0, 0), (1, 0)), None);
        // The rejection reason names both intervals (log + lamp evidence).
        let reason = protocol_no_intersection_reason((0, 0), (1, 1));
        assert!(reason.contains(REASON_PROTOCOL_NO_INTERSECTION), "{reason}");
        assert!(reason.contains("[0,0]"), "{reason}");
    }

    #[tokio::test]
    async fn capped_reader_rejects_oversized_frame_before_full_buffer() {
        // Newline-less input larger than the cap: the reader must error at
        // the cap, not buffer it all first.
        let rd = std::io::Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1]);
        let mut reader = CappedLineReader::new(rd);
        let r = reader.next_line().await;
        assert!(r.is_err(), "oversized frame must error, not buffer forever");
    }

    #[tokio::test]
    async fn capped_reader_yields_lines_and_eof() {
        let rd = std::io::Cursor::new(b"one\ntwo\n\n".to_vec());
        let mut reader = CappedLineReader::new(rd);
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("one"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("two"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some(""));
        assert_eq!(reader.next_line().await.unwrap(), None);
    }

    #[test]
    fn volume_receipts_update_the_single_device_snapshot() {
        let hub = DeviceHub::new();
        {
            let mut st = hub.state.lock().unwrap();
            st.devices.insert(
                "dot2-d".into(),
                DeviceSlot {
                    descriptor: HelloDevice {
                        stable_id: "dot2-d".into(),
                        model: "m".into(),
                        firmware: None,
                        capabilities: vec!["volume".into()],
                        audio_profile: "p".into(),
                        wifi_ssid: None,
                    },
                    generation: 1,
                    last_seq: 0,
                    online: true,
                    volume: None,
                    volume_at_ms: None,
                    volume_seq: 0,
                    aec_enabled: None,
                    aec_tail_ms: None,
                    aec_tail_samples: None,
                    aec_sample_rate: None,
                    aec_provenance: None,
                    profile_version: None,
                    echo_health: None,
                    echo_degraded_ms: None,
                    echo_health_reason: None,
                    northbound: None,
                    gateway_boot_id: None,
                    state_revision: None,
                    capability_fingerprint: None,
                    config_blocks: HashMap::new(),
                },
            );
            // An API task is attributed through volume_pending, even though
            // the v0.2 receipt has no device field.
            st.volume_pending.insert("P1".into(), "dot2-d".into());
            assert_eq!(
                record_volume(&mut st, "P1", 0.5, "device_confirmed", 7, None).as_deref(),
                Some("dot2-d")
            );
            assert_eq!(st.devices["dot2-d"].volume, Some(0.5));
            assert_eq!(st.devices["dot2-d"].volume_seq, 7);
            // A firmware key press / boot report: one device, so attributable.
            assert_eq!(
                record_volume(
                    &mut st,
                    "vol:dev:9",
                    124.0 / 127.0,
                    "device_confirmed",
                    8,
                    None
                )
                .as_deref(),
                Some("dot2-d")
            );
            // Out-of-range claims are ignored, not stored.
            st.volume_pending.insert("PX".into(), "dot2-d".into());
            assert!(record_volume(&mut st, "PX", 2.0, "device_confirmed", 9, None).is_none());
            assert!(
                !st.volume_pending.contains_key("PX"),
                "a malformed confirmed receipt ends the attempt"
            );
            assert_eq!(st.devices["dot2-d"].volume, Some(124.0 / 127.0));
            // R2-BM6 audit: a failed/unknown receipt must NOT move the
            // snapshot, even when its value happens to look valid.
            st.volume_pending.insert("P2".into(), "dot2-d".into());
            assert!(record_volume(&mut st, "P2", 0.9, "failed", 10, None).is_none());
            assert_eq!(st.devices["dot2-d"].volume, Some(124.0 / 127.0));
            assert_eq!(st.devices["dot2-d"].volume_seq, 8);
            assert!(
                !st.volume_pending.contains_key("P2"),
                "terminal receipt clears the pending map"
            );
        }
        let snap = hub.snapshot();
        let v = snap["devices"][0]["volume"].as_f64().unwrap();
        assert!((v - 124.0 / 127.0).abs() < 1e-9, "{snap}");
    }

    /// R2 maintenance (2026-11-13): two devices' physical volume keys must
    /// update their own snapshots. The receipt names its device, so no pending
    /// map or single-device guess is involved; an unknown name is ignored
    /// rather than creating a phantom device.
    #[test]
    fn a_volume_receipt_that_names_its_device_is_self_attributing() {
        let hub = DeviceHub::new();
        {
            let mut st = hub.state.lock().unwrap();
            for id in ["dot2-d", "dot2-e"] {
                st.devices.insert(
                    id.into(),
                    DeviceSlot {
                        descriptor: HelloDevice {
                            stable_id: id.into(),
                            model: "m".into(),
                            firmware: None,
                            capabilities: vec!["volume".into()],
                            audio_profile: "p".into(),
                            wifi_ssid: None,
                        },
                        generation: 1,
                        last_seq: 0,
                        online: true,
                        volume: None,
                        volume_at_ms: None,
                        volume_seq: 0,
                        aec_enabled: None,
                        aec_tail_ms: None,
                        aec_tail_samples: None,
                        aec_sample_rate: None,
                        aec_provenance: None,
                        profile_version: None,
                        echo_health: None,
                        echo_degraded_ms: None,
                        echo_health_reason: None,
                        northbound: None,
                        gateway_boot_id: None,
                        state_revision: None,
                        capability_fingerprint: None,
                        config_blocks: HashMap::new(),
                    },
                );
            }
            // Two physical key presses with no API task and no pending entry:
            // only the named device may move.
            assert_eq!(
                record_volume(
                    &mut st,
                    "vol:dev:1",
                    0.25,
                    "device_confirmed",
                    3,
                    Some("dot2-d")
                )
                .as_deref(),
                Some("dot2-d")
            );
            assert_eq!(
                record_volume(
                    &mut st,
                    "vol:dev:2",
                    0.75,
                    "device_confirmed",
                    4,
                    Some("dot2-e")
                )
                .as_deref(),
                Some("dot2-e")
            );
            assert_eq!(st.devices["dot2-d"].volume, Some(0.25));
            assert_eq!(st.devices["dot2-e"].volume, Some(0.75));
            // A bogus name is ignored, not stored anywhere.
            assert!(record_volume(
                &mut st,
                "vol:dev:3",
                0.5,
                "device_confirmed",
                5,
                Some("ghost")
            )
            .is_none());
            assert_eq!(st.devices["dot2-d"].volume, Some(0.25));
            // An OLD frame (no device) still falls back to the pending map.
            st.volume_pending.insert("P3".into(), "dot2-e".into());
            assert_eq!(
                record_volume(&mut st, "P3", 0.4, "device_confirmed", 6, None).as_deref(),
                Some("dot2-e")
            );
            assert_eq!(st.devices["dot2-e"].volume, Some(0.4));
        }
    }

    #[test]
    fn hub_rejects_unknown_device_queries() {
        let hub = DeviceHub::new();
        assert!(!hub.device_exists("ghost"));
        assert!(hub.stop_all_plays("ghost").is_empty());
    }

    #[test]
    fn v02_frames_round_trip() {
        let g = GatewayFrame::AudioChunk {
            origin: crate::gateway::InputOrigin::Legacy,
            device: "dot2-d".into(),
            ts: 1762500001123,
            seq: 1001,
            pcm_b64: "AAA=".into(),
            format: "s16le".into(),
            sample_rate: 16000,
            channels: 1,
            layout: "mono".into(),
            vad_speech: None,
            vad_prob: None,
        };
        let line = serde_json::to_string(&g).unwrap();
        assert!(line.starts_with("{\"type\":\"audio_chunk\""), "{line}");
        assert!(
            !line.contains("vad_speech"),
            "None vad fields omitted on wire: {line}"
        );
        assert_eq!(serde_json::from_str::<GatewayFrame>(&line).unwrap(), g);

        // Stage 2: AudioChunk with optional gateway neural VAD fields parses cleanly.
        let with_vad = r#"{"type":"audio_chunk","device":"lx06","ts":1,"seq":2,"pcm_b64":"AAA=","format":"s16le","sample_rate":16000,"channels":1,"layout":"mono","vad_speech":true,"vad_prob":0.91}"#;
        match serde_json::from_str::<GatewayFrame>(with_vad).unwrap() {
            GatewayFrame::AudioChunk {
                vad_speech,
                vad_prob,
                ..
            } => {
                assert_eq!(vad_speech, Some(true));
                assert_eq!(vad_prob, Some(0.91));
            }
            other => panic!("expected AudioChunk, got {other:?}"),
        }
        // Stage 2 resolution precedence & finite threshold handling:
        assert_eq!(resolve_remote_vad(Some(false), Some(0.95)), Some(false));
        assert_eq!(resolve_remote_vad(Some(true), Some(0.01)), Some(true));
        assert_eq!(resolve_remote_vad(None, Some(0.50)), Some(true));
        assert_eq!(resolve_remote_vad(None, Some(0.49)), Some(false));
        assert_eq!(resolve_remote_vad(None, Some(f32::NAN)), None);
        assert_eq!(resolve_remote_vad(None, Some(f32::INFINITY)), None);
        assert_eq!(resolve_remote_vad(None, Some(f32::NEG_INFINITY)), None);
        assert_eq!(resolve_remote_vad(None, None), None);

        let g = GatewayFrame::MicReport {
            device: "dot2-d".into(),
            active: true,
            layout: Some("mono".into()),
            sample_rate: Some(16000),
            channels: Some(1),
            detail: None,
        };
        assert!(serde_json::to_string(&g)
            .unwrap()
            .starts_with("{\"type\":\"mic_report\""));

        let g = GatewayFrame::DeviceStatus {
            northbound: None,
            device: "dot2-d".into(),
            online: true,
            reason: "registered".into(),
            descriptor: None,
        };
        assert!(serde_json::to_string(&g)
            .unwrap()
            .starts_with("{\"type\":\"device_status\""));

        let c = CoreFrame::MicSession {
            device: "dot2-d".into(),
            action: "start".into(),
            mode: Some("continuous".into()),
        };
        let line = serde_json::to_string(&c).unwrap();
        assert!(line.starts_with("{\"type\":\"mic_session\""), "{line}");
        assert_eq!(serde_json::from_str::<CoreFrame>(&line).unwrap(), c);

        let c = CoreFrame::AssistantState {
            device: "dot2-d".into(),
            state: "listening".into(),
        };
        let line = serde_json::to_string(&c).unwrap();
        assert!(line.starts_with("{\"type\":\"assistant_state\""), "{line}");
        assert_eq!(serde_json::from_str::<CoreFrame>(&line).unwrap(), c);
    }

    /// Two speakers may never share one id: the hub keys every per-device map
    /// by it, so an alias would corrupt voice state silently (audit 2026-11-14).
    #[test]
    fn duplicate_device_ids_in_one_hello_are_refused() {
        let dev = |id: &str| HelloDevice {
            stable_id: id.into(),
            model: "AEOBC".into(),
            firmware: None,
            capabilities: vec!["audio_in".into()],
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: None,
        };
        assert!(ensure_unique_device_ids(&[dev("dot2-d"), dev("dot2-e")]).is_ok());
        let err = ensure_unique_device_ids(&[dev("dot2-d"), dev("dot2-d")]).unwrap_err();
        assert!(err.contains("duplicate device id"), "{err}");
        assert!(ensure_unique_device_ids(&[]).is_ok());
    }

    #[test]
    fn gateway_bind_must_be_loopback() {
        assert!(ensure_loopback_bind("127.0.0.1:8932".parse().unwrap()).is_ok());
        assert!(ensure_loopback_bind("[::1]:8932".parse().unwrap()).is_ok());
        let lan = ensure_loopback_bind("192.168.1.34:8932".parse().unwrap()).unwrap_err();
        assert!(lan.contains("not loopback"), "{lan}");
        let any = ensure_loopback_bind("0.0.0.0:8932".parse().unwrap()).unwrap_err();
        assert!(any.contains("not loopback"), "{any}");
    }
}

#[cfg(test)]
mod d0_source_tests {
    use super::*;

    #[test]
    fn d0_raw_json_keeps_source_and_arrival_identity() {
        let hub = DeviceHub::new();
        hub.state.lock().unwrap().generation = 7;
        let mut ctrl_rx = hub.subscribe_control();
        let mut audio_rx = hub.subscribe_audio();
        let audio = json!({"kind":"audio", "capture_epoch":"boot-a", "ticket":"capture-a",
            "input_id":"input-a", "start_sample":1280, "end_sample":2560});
        use base64::Engine;
        let pcm_b64 = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 2560]);
        for (index, kind) in ["wake", "utterance", "keyword", "audio_chunk"]
            .iter()
            .enumerate()
        {
            let origin = if *kind == "utterance" {
                json!({"kind":"text", "input_id":"text-a"})
            } else {
                audio.clone()
            };
            let frame = json!({"type":kind, "device":"dot2-d", "ts":99999, "seq":index+1,
                "source":"text", "text":"same text", "keyword":"same text", "origin":origin,
                "pcm_b64":pcm_b64, "format":"s16le", "sample_rate":16000, "channels":1, "layout":"mono"});
            hub.handle_frame(7, serde_json::from_value(frame).unwrap());
            // Advance the hub while this event is still queued.
            hub.state.lock().unwrap().generation = 8;
            let event = if *kind == "audio_chunk" {
                audio_rx.try_recv().unwrap()
            } else {
                ctrl_rx.try_recv().unwrap()
            };
            let provenance = match event {
                DeviceEvent::Wake { provenance, .. }
                | DeviceEvent::Utterance { provenance, .. }
                | DeviceEvent::Keyword { provenance, .. }
                | DeviceEvent::AudioChunk { provenance, .. } => provenance,
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(serde_json::to_value(&provenance.origin).unwrap(), origin);
            assert_eq!(provenance.transport.as_ref().unwrap().generation, 7);
            assert_eq!(
                provenance.transport.as_ref().unwrap().inbound_seq,
                index as u64 + 1
            );
            // Consuming an old queued event must never borrow a newer connection.
            assert_eq!(provenance.transport.unwrap().generation, 7);
            hub.state.lock().unwrap().generation = 7;
        }
        hub.handle_frame(
            7,
            serde_json::from_value(json!({"type":"wake", "device":"dot2-d", "ts":1,"seq":100}))
                .unwrap(),
        );
        let DeviceEvent::Wake { provenance, .. } = ctrl_rx.try_recv().unwrap() else {
            panic!("wake")
        };
        assert_eq!(provenance.origin, InputOrigin::Legacy);
    }

    #[test]
    fn hello_echo_health_and_near_end_presence_handle_defaults() {
        let hello_old: GatewayFrame = serde_json::from_value(json!({
            "type": "hello",
            "protocol": 1,
            "gateway": "old-gw",
            "devices": []
        }))
        .unwrap();
        match hello_old {
            GatewayFrame::Hello {
                aec_tail_ms,
                profile_version,
                ..
            } => {
                assert_eq!(aec_tail_ms, None);
                assert_eq!(profile_version, None);
            }
            _ => panic!("expected Hello"),
        }

        let echo: GatewayFrame = serde_json::from_value(json!({
            "type": "echo_health",
            "device": "dot2-d",
            "health": "degraded"
        }))
        .unwrap();
        match echo {
            GatewayFrame::EchoHealth {
                device,
                health,
                degraded_ms,
                reason,
            } => {
                assert_eq!(device, "dot2-d");
                assert_eq!(health, "degraded");
                assert_eq!(degraded_ms, 0);
                assert_eq!(reason, None);
            }
            _ => panic!("expected EchoHealth"),
        }

        let presence: GatewayFrame = serde_json::from_value(json!({
            "type": "near_end_presence",
            "device": "dot2-d"
        }))
        .unwrap();
        match presence {
            GatewayFrame::NearEndPresence {
                device,
                clipped,
                snr_db,
                ..
            } => {
                assert_eq!(device, "dot2-d");
                assert!(!clipped);
                assert_eq!(snr_db, None);
            }
            _ => panic!("expected NearEndPresence"),
        }
    }

    #[test]
    fn stop_and_wait_rejects_stale_connection_generation_without_sending() {
        let hub = DeviceHub::new();
        let (tx, mut rx) = outbound::outbound_channel();
        hub.attach_test_outbound(2, tx);
        {
            let mut st = hub.lock_state();
            st.active_plays.insert(
                "P-stale".into(),
                ActivePlay {
                    device: "dot2-d".into(),
                    channel: "voice".into(),
                    generation: 1,
                },
            );
        }
        match hub.stop_and_wait("P-stale") {
            StopSubmission::SendFailed { reason } => {
                assert!(
                    reason.contains("stale_connection_generation"),
                    "expected stale_connection_generation, got: {reason}"
                );
            }
            StopSubmission::NoActivePlay => panic!("expected SendFailed, got NoActivePlay"),
            StopSubmission::Accepted { .. } => panic!("expected SendFailed, got Accepted"),
        }
        assert!(
            rx.try_recv().is_err(),
            "no Stop frame may be sent onto the newer generation"
        );
        assert!(
            hub.lock_state().stop_waiters.is_empty(),
            "no waiter may be left behind"
        );
    }

    #[test]
    fn keyword_drops_stale_kws_generation_and_accepts_current_or_legacy() {
        let hub = DeviceHub::new();
        let mut rx = hub.subscribe();
        {
            let mut st = hub.lock_state();
            st.generation = 1;
            let mut slot = DeviceSlot {
                descriptor: HelloDevice {
                    stable_id: "dot2-d".into(),
                    model: "dot2".into(),
                    firmware: Some("0.0.53".into()),
                    capabilities: vec![],
                    audio_profile: "dot2".into(),
                    wifi_ssid: None,
                },
                generation: 1,
                last_seq: 0,
                online: true,
                volume: None,
                volume_at_ms: None,
                volume_seq: 0,
                aec_enabled: None,
                aec_tail_ms: None,
                aec_tail_samples: None,
                aec_sample_rate: None,
                aec_provenance: None,
                profile_version: None,
                echo_health: None,
                echo_degraded_ms: None,
                echo_health_reason: None,
                northbound: None,
                gateway_boot_id: None,
                state_revision: None,
                capability_fingerprint: None,
                config_blocks: HashMap::new(),
            };
            slot.config_blocks.insert(
                ConfigBlock::Kws,
                ConfigBlockState {
                    block: ConfigBlock::Kws,
                    block_generation: 5,
                    operation_id: "op-5".into(),
                    data_epoch: 1,
                    persisted_hash: Some("h5".into()),
                    active_hash: Some("h5".into()),
                    active_values: Some(json!({})),
                    evidence: Some(json!({})),
                    state: ConfigReceiptStatus::Active,
                    reason: None,
                },
            );
            st.devices.insert("dot2-d".into(), slot);
        }

        let mk_kw = |kw: &str, kws_generation: Option<u64>| GatewayFrame::Keyword {
            device: "dot2-d".into(),
            ts: 1000,
            seq: 1,
            keyword: kw.into(),
            fed_ms: Some(800),
            self_heard: false,
            rms_dbfs: Some(-24.0),
            lag_ms: Some(12),
            origin: InputOrigin::Legacy,
            bundle_id: Some("b5".into()),
            kws_generation,
            kws_tier: None,
            wake_dur_ms: None,
            max_gap_ms: None,
            l2_passed: None,
        };

        // Older KWS generation (4 < 5) must be dropped and recorded as stale_kws_keyword.
        hub.handle_frame(1, mk_kw("stale-word", Some(4)));
        assert!(
            rx.try_recv().is_err(),
            "stale KWS generation keyword must be dropped"
        );
        assert!(
            hub.lock_state()
                .ring
                .iter()
                .any(|e| e["kind"] == "stale_kws_keyword"),
            "stale_kws_keyword must be recorded"
        );

        // Current KWS generation (5 >= 5) must pass through.
        hub.handle_frame(1, mk_kw("current-word", Some(5)));
        match rx.try_recv().expect("current generation keyword emitted") {
            DeviceEvent::Keyword { keyword, .. } => assert_eq!(keyword, "current-word"),
            other => panic!("expected DeviceEvent::Keyword, got {other:?}"),
        }

        // Legacy gateway (kws_generation == None) remains backward compatible.
        hub.handle_frame(1, mk_kw("legacy-word", None));
        match rx.try_recv().expect("legacy keyword emitted") {
            DeviceEvent::Keyword { keyword, .. } => assert_eq!(keyword, "legacy-word"),
            other => panic!("expected DeviceEvent::Keyword, got {other:?}"),
        }
    }

    /// #233 P1-3: quantitative slow-consumer regression test.
    /// 1) Under a stalled consumer, pushing 1,000 audio chunks (~8 KiB PCM each)
    ///    is strictly bounded at `OUTBOUND_PLAY_CAP` (512 frames), preventing
    ///    unbounded queue/memory growth and surfacing `outbound_backpressure` +
    ///    `outbound_queue` metrics.
    /// 2) Even when the play queue is 100% full (512/512), a control frame
    ///    (`Stop` via `stop_and_wait`) is admitted onto the dedicated control
    ///    tier (`OUTBOUND_CONTROL_CAP = 256`) and dequeued by `rx.recv().await`
    ///    as the VERY FIRST frame (< 5 ms control latency, zero head-of-line
    ///    blocking behind queued audio chunks).
    /// 3) Subsequent play frames preserve strict `PlayBegin -> PlayChunk(0..)`
    ///    FIFO order.
    #[tokio::test]
    async fn slow_consumer_bounds_play_backlog_and_prioritizes_control_frames_in_o1() {
        use super::transport_metrics::{OUTBOUND_CONTROL_CAP, OUTBOUND_PLAY_CAP};
        let hub = DeviceHub::new();
        let (tx, mut rx) =
            outbound::outbound_channel_with_metrics(hub.transport_tasks.outbound.clone());
        hub.attach_test_outbound(1, tx);
        {
            let mut st = hub.lock_state();
            st.device_to_conn.insert("dot2-d".into(), 1);
            st.devices.insert(
                "dot2-d".into(),
                DeviceSlot {
                    descriptor: HelloDevice {
                        stable_id: "dot2-d".into(),
                        model: "dot2".into(),
                        firmware: Some("0.0.53".into()),
                        capabilities: vec!["audio_out".into(), "play_stream".into()],
                        audio_profile: "dot2".into(),
                        wifi_ssid: None,
                    },
                    generation: 1,
                    last_seq: 0,
                    online: true,
                    volume: None,
                    volume_at_ms: None,
                    volume_seq: 0,
                    aec_enabled: None,
                    aec_tail_ms: None,
                    aec_tail_samples: None,
                    aec_sample_rate: None,
                    aec_provenance: None,
                    profile_version: None,
                    echo_health: None,
                    echo_degraded_ms: None,
                    echo_health_reason: None,
                    northbound: None,
                    gateway_boot_id: None,
                    state_revision: None,
                    capability_fingerprint: None,
                    config_blocks: HashMap::new(),
                },
            );
        }

        let ticket = hub
            .play_begin("dot2-d", "media", 24_000, Some("slow-consumer".into()))
            .expect("play_begin admitted into empty play queue");
        let play_id = ticket.play_id;
        let pcm_chunk = vec![0x55u8; 8192];
        let mut accepted_chunks = 0usize;
        let mut rejected_chunks = 0usize;
        for seq in 0..1000u64 {
            match hub.play_chunk(&play_id, seq, &pcm_chunk) {
                Ok(()) => accepted_chunks += 1,
                Err(e) => {
                    assert!(
                        e.contains("gateway outbound play queue full"),
                        "unexpected backpressure error: {e}"
                    );
                    rejected_chunks += 1;
                }
            }
        }
        // 1 PlayBegin + 511 PlayChunks = 512 (`OUTBOUND_PLAY_CAP`), remaining 489 rejected.
        assert_eq!(1 + accepted_chunks, OUTBOUND_PLAY_CAP);
        assert_eq!(rejected_chunks, 1000 - (OUTBOUND_PLAY_CAP - 1));

        let snap = hub.snapshot();
        assert_eq!(
            snap["outbound_queue"]["play_capacity"],
            json!(OUTBOUND_PLAY_CAP)
        );
        assert_eq!(
            snap["outbound_queue"]["control_capacity"],
            json!(OUTBOUND_CONTROL_CAP)
        );
        assert_eq!(
            snap["outbound_queue"]["play_queued"],
            json!(OUTBOUND_PLAY_CAP)
        );
        assert_eq!(
            snap["outbound_queue"]["play_high_water"],
            json!(OUTBOUND_PLAY_CAP)
        );
        assert_eq!(
            snap["outbound_queue"]["play_dropped"],
            json!(rejected_chunks)
        );
        assert!(
            hub.recent(5)
                .iter()
                .any(|e| e["kind"] == "outbound_backpressure" && e["tier"] == "play"),
            "outbound_backpressure ring event must be recorded when play queue saturates"
        );

        // Control frame priority under 100% full play queue: Stop is admitted
        // and dequeued FIRST in O(1) without waiting for 512 play frames.
        let t0 = std::time::Instant::now();
        match hub.stop_and_wait(&play_id) {
            StopSubmission::Accepted { play_id: id, .. } => assert_eq!(id, play_id),
            other => panic!(
                "Stop must succeed on control tier even when play tier is full, got {}",
                match other {
                    StopSubmission::NoActivePlay => "NoActivePlay".to_string(),
                    StopSubmission::SendFailed { reason } => format!("SendFailed({reason})"),
                    StopSubmission::Accepted { .. } => unreachable!(),
                }
            ),
        }
        let first_dequeued = rx.recv().await.expect("first dequeued frame");
        let ctrl_latency = t0.elapsed();
        assert!(
            matches!(&first_dequeued, CoreFrame::Stop { task_id, flush: true } if task_id == &play_id),
            "biased control tier must dequeue Stop ahead of 512 backed-up play frames, got {first_dequeued:?}"
        );
        assert!(
            ctrl_latency < std::time::Duration::from_millis(50),
            "control frame dequeue latency under saturated play queue must be O(1), got {ctrl_latency:?}"
        );

        // Next dequeued frames from play_rx preserve strict FIFO order: PlayBegin -> PlayChunk(0) -> PlayChunk(1).
        let f_begin = rx.recv().await.expect("PlayBegin");
        assert!(matches!(f_begin, CoreFrame::PlayBegin { ref task_id, .. } if task_id == &play_id));
        let f_c0 = rx.recv().await.expect("PlayChunk 0");
        assert!(
            matches!(f_c0, CoreFrame::PlayChunk { ref task_id, seq: 0, .. } if task_id == &play_id)
        );
        let f_c1 = rx.recv().await.expect("PlayChunk 1");
        assert!(
            matches!(f_c1, CoreFrame::PlayChunk { ref task_id, seq: 1, .. } if task_id == &play_id)
        );
    }

    #[test]
    fn gateway_bind_policy_enforces_loopback_by_default_and_allows_lan_opt_in() {
        let lo_v4: std::net::SocketAddr = "127.0.0.1:8932".parse().unwrap();
        let lo_v6: std::net::SocketAddr = "[::1]:8932".parse().unwrap();
        let any_v4: std::net::SocketAddr = "0.0.0.0:8932".parse().unwrap();
        let lan_v4: std::net::SocketAddr = "192.168.1.27:8932".parse().unwrap();

        // Default embedded speaker policy (allow_lan = false)
        assert!(ensure_loopback_bind(lo_v4).is_ok());
        assert!(ensure_loopback_bind(lo_v6).is_ok());
        assert!(ensure_loopback_bind(any_v4).is_err());
        assert!(ensure_loopback_bind(lan_v4).is_err());
        assert!(ensure_gateway_bind_with_policy(lo_v4, false).is_ok());
        assert!(ensure_gateway_bind_with_policy(any_v4, false).is_err());
        assert!(ensure_gateway_bind_with_policy(lan_v4, false).is_err());

        // Opt-in Docker/NAS LAN policy (allow_lan = true via HM_GATEWAY_ALLOW_LAN=1)
        assert!(ensure_gateway_bind_with_policy(lo_v4, true).is_ok());
        assert!(ensure_gateway_bind_with_policy(any_v4, true).is_ok());
        assert!(ensure_gateway_bind_with_policy(lan_v4, true).is_ok());
    }

    #[test]
    fn resolve_gateway_bind_promotes_paired_lan_speaker_and_keeps_unpaired_or_loopback_closed() {
        let http_lan: std::net::SocketAddr = "0.0.0.0:8930".parse().unwrap();
        let http_lo: std::net::SocketAddr = "127.0.0.1:8930".parse().unwrap();
        let gw_lo: std::net::SocketAddr = "127.0.0.1:8932".parse().unwrap();
        let gw_custom_lo: std::net::SocketAddr = "127.0.0.1:55145".parse().unwrap();
        let gw_any: std::net::SocketAddr = "0.0.0.0:8932".parse().unwrap();

        // 1. Paired LAN speaker with legacy supervisor HM_GATEWAY_BIND=127.0.0.1:8932 or unset promotes to 0.0.0.0:8932.
        assert_eq!(
            resolve_gateway_bind(http_lan, Some(gw_lo), false, false, true).unwrap(),
            gw_any
        );
        assert_eq!(
            resolve_gateway_bind(http_lan, None, false, false, true).unwrap(),
            gw_any
        );
        // Custom non-8932 loopback port stays untouched even on a paired LAN Core.
        assert_eq!(
            resolve_gateway_bind(http_lan, Some(gw_custom_lo), false, false, true).unwrap(),
            gw_custom_lo
        );
        // 2. Explicit HM_GATEWAY_LOCK_LOOPBACK=1 keeps it on 127.0.0.1:8932 and rejects explicit non-loopback.
        assert_eq!(
            resolve_gateway_bind(http_lan, Some(gw_lo), false, true, true).unwrap(),
            gw_lo
        );
        assert_eq!(
            resolve_gateway_bind(http_lan, None, true, true, true).unwrap(),
            gw_lo
        );
        assert!(resolve_gateway_bind(http_lan, Some(gw_any), true, true, true).is_err());
        // 3. Unpaired speaker stays fail-closed on 127.0.0.1:8932 and rejects explicit 0.0.0.0:8932.
        assert_eq!(
            resolve_gateway_bind(http_lan, Some(gw_lo), false, false, false).unwrap(),
            gw_lo
        );
        assert!(resolve_gateway_bind(http_lan, Some(gw_any), false, false, false).is_err());
        // 4. Unpaired NAS Docker with HM_GATEWAY_ALLOW_LAN=1 binds 0.0.0.0:8932.
        assert_eq!(
            resolve_gateway_bind(http_lan, None, true, false, false).unwrap(),
            gw_any
        );
        // 5. Loopback-only Core (dev/tests) never promotes even when paired.
        assert_eq!(
            resolve_gateway_bind(http_lo, Some(gw_lo), false, false, true).unwrap(),
            gw_lo
        );
        assert_eq!(
            resolve_gateway_bind(http_lo, None, false, false, true).unwrap(),
            gw_lo
        );
    }
}
