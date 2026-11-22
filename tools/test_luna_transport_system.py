"""Tests of the external fault mechanism, not a replacement for paired runs."""
import asyncio
import collections
import io
import unittest

from luna_transport_system import FaultProxy, ProbeClock, check_bounds, check_progress, cpu_seconds, latest_probe, observe_consumers, percentile
from system_smoke import SmokeFailure


class ProxyTests(unittest.IsolatedAsyncioTestCase):
    async def test_shutdown_cancels_held_tunnel_before_waiting_for_server_close(self):
        connected = asyncio.Event()
        async def peer(reader, writer):
            try:
                connected.set()
                await reader.read()
            finally:
                writer.close()
        server = await asyncio.start_server(peer, "127.0.0.1", 0)
        proxy = FaultProxy(server.sockets[0].getsockname()[1])
        await proxy.start()
        reader, writer = await asyncio.open_connection("127.0.0.1", proxy.port)
        try:
            await asyncio.wait_for(connected.wait(), 1)
            self.assertIsNotNone(proxy.pause_current(120))
            writer.write(b"held")
            await writer.drain()
            await asyncio.wait_for(proxy.close(), 1)
            self.assertEqual(await asyncio.wait_for(reader.read(), 1), b"")
            self.assertFalse(proxy.tasks)
            self.assertFalse(proxy.tunnels)
        finally:
            writer.close()
            server.close()
            await server.wait_closed()

    async def test_old_connection_pauses_but_fresh_connection_and_resume_forward_exact_bytes(self):
        async def echo(reader, writer):
            try:
                while data := await reader.read(4096):
                    writer.write(data)
                    await writer.drain()
            finally:
                writer.close()

        server = await asyncio.start_server(echo, "127.0.0.1", 0)
        proxy = FaultProxy(server.sockets[0].getsockname()[1])
        await proxy.start()
        writers = []
        try:
            first, writer = await asyncio.open_connection("127.0.0.1", proxy.port)
            writers.append(writer)
            writer.write(b"before")
            await writer.drain()
            self.assertEqual(await asyncio.wait_for(first.readexactly(6), 1), b"before")
            paused = proxy.pause_current(60)
            self.assertIsNotNone(paused)
            writer.write(b"held\x00\xff\n")
            await writer.drain()
            with self.assertRaises(asyncio.TimeoutError):
                await asyncio.wait_for(first.readexactly(7), .05)
            fresh, fresh_writer = await asyncio.open_connection("127.0.0.1", proxy.port)
            writers.append(fresh_writer)
            fresh_writer.write(b"new connection")
            await fresh_writer.drain()
            self.assertEqual(await asyncio.wait_for(fresh.readexactly(14), 1), b"new connection")
            proxy.tunnels[paused["connection"]].gate.set()
            self.assertEqual(await asyncio.wait_for(first.readexactly(7), 1), b"held\x00\xff\n")
        finally:
            for writer in writers:
                writer.close()
            await proxy.close()
            server.close()
            await server.wait_closed()
        self.assertEqual(len(proxy.tasks), 0)
        self.assertEqual(len(proxy.tunnels), 0)


class ObservationTests(unittest.TestCase):
    def test_health_readiness_requires_matching_real_snapshot_and_survives_reconnect(self):
        node = {}
        device = {"stable_id": "fixture-0", "echo_health": "unknown",
                  "echo_health_reason": "aec_disabled", "echo_degraded_ms": 0}
        diag = {"gateway": {"devices": [device], "generation": 1},
                "gateway_counts": {"echo_health": 1}}
        observe_consumers(node, diag, "fixture-1")
        self.assertNotIn("health_consumer_seen", node)
        for missing in ("echo_health", "echo_health_reason", "echo_degraded_ms"):
            bad = dict(device)
            del bad[missing]
            observe_consumers(node, dict(diag, gateway={"devices": [bad], "generation": 1}), "fixture-0")
            self.assertNotIn("health_consumer_seen", node)
        observe_consumers(node, diag, "fixture-0")
        fresh = {"devices": [{"stable_id": "fixture-0", "echo_health": "unknown"}], "generation": 2}
        observe_consumers(node, dict(diag, gateway=fresh), "fixture-0")
        self.assertTrue(node["health_consumer_seen"])
        self.assertEqual(node["health_consumer_snapshot"]["generation"], 1)
        self.assertEqual(node["health_consumer_snapshot"]["device"], device)

    def test_stalled_observer_does_not_turn_repeated_reads_into_fresh_evidence(self):
        clock = ProbeClock()
        clock.accept({"elapsed_ms": 0}, 10.)
        clock.check_fresh(13.)
        clock.accept({"elapsed_ms": 0}, 13.)
        self.assertEqual(clock.unique, 1)
        with self.assertRaises(SmokeFailure):
            clock.check_fresh(13.001)
        clock.accept({"elapsed_ms": 1000}, 14.)
        clock.check_fresh(14.)
        self.assertEqual(clock.unique, 2)
        with self.assertRaises(SmokeFailure):
            clock.accept({"elapsed_ms": 500}, 15.)
        records = []
        latest_probe(io.StringIO('{"elapsed_ms":1}\n{"elapsed_ms":2}\n'), {}, records.append)
        self.assertEqual([r["elapsed_ms"] for r in records], [1, 2])

    def test_cadence_and_actual_fault_clocks_are_not_replaced_by_proxy_pause(self):
        initial = {"capture_calls": 0, "playback_calls": 0, "elapsed_ms": 0,
            "capture_max_gap_us": 250000, "playback_max_gap_us": 150000,
            "producer_max_us": 20000, "reader_tasks": 1, "writer_tasks": 1,
            "decode_active": 1, "decode_pending": 64, "last_fault_generation": 0}
        good = dict(initial, elapsed_ms=10000, capture_calls=112, playback_calls=125)
        history = collections.deque()
        check_progress(initial, history, True)
        check_progress(good, history, True)
        for field, value in [("capture_calls", 111), ("playback_calls", 124),
                             ("producer_max_us", 20001), ("capture_max_gap_us", 250001),
                             ("playback_max_gap_us", 150001), ("writer_tasks", 2), ("decode_pending", 65)]:
            with self.subTest(field=field), self.assertRaises(SmokeFailure):
                check_progress(dict(good, **{field: value}), collections.deque([initial]), True)
        fault = dict(good, last_fault_generation=1, last_cleanup_generation=1,
            last_fault_write_started_us=1000000, last_fault_us=2001000, last_cleanup_us=2001047,
            northbound={"last_fault": {"code": "writer_timeout"}})
        check_progress(fault, collections.deque(), True)
        for changes in [{"last_fault_us": 2100001, "last_cleanup_us": 2100048},
                        {"last_cleanup_us": 3001001}, {"last_cleanup_generation": 0}]:
            with self.subTest(changes=changes), self.assertRaises(SmokeFailure):
                check_progress(dict(fault, **changes), collections.deque(), True)

    def test_cumulative_cpu_time_accounts_for_minutes_hours_and_days(self):
        self.assertEqual(cpu_seconds("1:02.30"), 62.3)
        self.assertEqual(cpu_seconds("2:01:02"), 7262)
        self.assertEqual(cpu_seconds("1-00:00:01"), 86401)

    def test_partial_metric_line_is_retried_without_accepting_or_losing_it(self):
        source = io.StringIO('{"a":1}\n{"a":2')
        self.assertEqual(latest_probe(source, {}), {"a": 1})
        offset = source.tell()
        source.seek(0, io.SEEK_END)
        source.write('}\n')
        source.seek(offset)
        self.assertEqual(latest_probe(source, {}), {"a": 2})

    def test_every_count_and_byte_guard_rejects_its_own_overflow(self):
        limits = {"critical_current_count": 64, "critical_high_water_count": 64,
                  "critical_current_bytes": 262144, "critical_high_water_bytes": 262144,
                  "audio_current_count": 25, "audio_high_water_count": 25}
        observer = {"pending_health_count": 1, "pending_presence_count": 1,
                    "pending_health_bytes": 4096, "pending_presence_bytes": 4096,
                    "inflight_bytes": 262144}
        self.assertTrue(check_bounds(dict(observer, northbound=limits)))
        for key, value in limits.items():
            with self.subTest(key=key):
                bad = dict(limits, **{key: value + 1})
                with self.assertRaises(SmokeFailure):
                    check_bounds(dict(observer, northbound=bad))
        for key, value in observer.items():
            with self.subTest(key=key):
                with self.assertRaises(SmokeFailure):
                    check_bounds(dict(observer, **{key: value + 1}, northbound=limits))
        self.assertFalse(check_bounds({}))
        self.assertFalse(check_bounds({"northbound": None}))

    def test_percentiles_use_nearest_rank_without_discarding_slow_samples(self):
        self.assertEqual(percentile([1] * 19 + [1000], .95), 1)
        self.assertEqual(percentile([1] * 18 + [999, 1000], .95), 999)


if __name__ == "__main__":
    unittest.main()
