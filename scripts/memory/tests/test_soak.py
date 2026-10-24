import json
import os
import tempfile
import unittest
from unittest import mock

from scripts.memory import soak, stats, status

MIB = 1024 * 1024


class ScopeTests(unittest.TestCase):
    def test_bench_proxy_is_rejected_without_explicit_surrogate(self):
        with self.assertRaises(soak.SoakError):
            soak._target_scope("bench-proxy", "target/debug/bench-proxy", False)

    def test_bench_proxy_surrogate_is_labeled_not_full_edge(self):
        scope = soak._target_scope("bench-proxy", "target/debug/bench-proxy", True)
        self.assertFalse(scope["full_edge"])
        self.assertIn("surrogate", scope["kind"])

    def test_bench_defense_is_a_component_lane_not_full_edge(self):
        scope = soak._target_scope("bench-defense", "target/debug/bench-defense", False)
        self.assertFalse(scope["full_edge"])
        self.assertTrue(scope["component_lane"])
        self.assertEqual(scope["kind"], "bench-defense-component")
        self.assertIn("COMPONENT LANE ONLY", scope["note"])

    def test_full_edge_requires_the_production_binary(self):
        with self.assertRaises(soak.SoakError):
            soak._target_scope("full-edge", "target/debug/bench-defense", False)
        scope = soak._target_scope("full-edge", "opt/zelvik/Zelvik-Edge", False)
        self.assertTrue(scope["full_edge"])
        self.assertTrue(scope["production_startup"])
        self.assertFalse(scope["component_lane"])


class StatsReadinessTests(unittest.TestCase):
    KEYS = stats.DEFAULT_REQUIRED_KEYS

    def write(self, path):
        document = {
            "process": {
                "commit_current": 1,
                "peak_commit": 1,
                "rss_current": 1,
                "page_faults": 1,
            }
        }
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(document, handle)

    def test_complete_document_is_ready_immediately(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "alloc-stats.json")
            self.write(path)
            ready = soak._wait_stats_ready(path, self.KEYS, 0)
        self.assertEqual(ready["process.rss_current"], 1.0)

    def test_missing_source_fails_within_the_bound(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "alloc-stats.json")
            with self.assertRaises(soak.SoakError) as caught:
                soak._wait_stats_ready(path, self.KEYS, 0)
        self.assertIn("did not become ready within 0s", str(caught.exception))
        self.assertIn("does not exist", str(caught.exception))

    def test_incomplete_document_is_retried_until_ready(self):
        with (
            mock.patch.object(
                soak.stats,
                "read",
                side_effect=[stats.StatsError("empty"), {"process.rss_current": 1.0}],
            ) as read,
            mock.patch.object(soak.time, "sleep"),
        ):
            ready = soak._wait_stats_ready("alloc-stats.json", self.KEYS, 1)
        self.assertEqual(ready["process.rss_current"], 1.0)
        self.assertEqual(read.call_count, 2)


class AnalyzeTests(unittest.TestCase):
    def test_flat_series_passes(self):
        samples = [(i * 1.0, 100 * MIB) for i in range(200)]
        result = soak.analyze(
            samples,
            warmup_seconds=10,
            budget_mib_per_hour=16,
            min_span_seconds=60,
            last_pressure_end=None,
            pressure_settle_seconds=10,
        )
        self.assertTrue(result["ok"])
        self.assertLess(result["upper95_mib_per_hour"], 16)

    def test_growth_fails(self):
        samples = [(i * 1.0, 100 * MIB + i * MIB) for i in range(200)]
        result = soak.analyze(
            samples,
            warmup_seconds=10,
            budget_mib_per_hour=16,
            min_span_seconds=60,
            last_pressure_end=None,
            pressure_settle_seconds=10,
        )
        self.assertFalse(result["ok"])
        self.assertIn("slope gate", f"slope gate failed: {result.get('error')}")

    def test_pressure_window_is_excluded_from_warmup(self):
        # 100s of growth, then 100s flat after the pressure cycles ended.
        samples = [(float(i), 100 * MIB + i * 10 * MIB) for i in range(100)]
        samples += [(float(i), 1100 * MIB) for i in range(100, 200)]
        result = soak.analyze(
            samples,
            warmup_seconds=0,
            budget_mib_per_hour=4,
            min_span_seconds=60,
            last_pressure_end=100.0,
            pressure_settle_seconds=10,
        )
        self.assertTrue(result["ok"], result)

    def test_anon_series_is_gated_separately_from_rss(self):
        # Flat total RSS, 4.5 MiB/h anonymous growth. The combined verdict must
        # fail even though the RSS series alone is clean.
        rss = [(float(i * 10), 100 * MIB) for i in range(400)]
        anon = [
            (float(i * 10), 50 * MIB + i * 10 * 4.5 * MIB / 3600.0)
            for i in range(400)
        ]
        result = soak.analyze_pair(
            rss,
            anon,
            warmup_seconds=0,
            budget_mib_per_hour=0.5,
            anon_budget_mib_per_hour=0.5,
            min_span_seconds=3600,
            last_pressure_end=None,
            pressure_settle_seconds=0,
        )
        self.assertFalse(result["ok"])
        self.assertTrue(result["rss"]["ok"], result["rss"])
        self.assertFalse(result["anon"]["ok"], result["anon"])
        self.assertEqual(result["rss"]["label"], "RSS")
        self.assertEqual(result["anon"]["label"], "Anon")
        # The Anon failure must report the Anon series, not the RSS label or
        # values. These series differ on purpose.
        self.assertIn("Anon slope", result["anon"]["error"])
        self.assertNotIn("RSS", result["anon"]["error"])
        self.assertIn("0.500 MiB/h", result["anon"]["error"])
        self.assertNotEqual(
            result["rss"]["upper95_mib_per_hour"],
            result["anon"]["upper95_mib_per_hour"],
        )
        self.assertNotEqual(
            result["rss"]["theil_sen_mib_per_hour"],
            result["anon"]["theil_sen_mib_per_hour"],
        )

    def test_flat_rss_and_anon_pass(self):
        points = [(float(i * 10), 100 * MIB) for i in range(400)]
        result = soak.analyze_pair(
            points,
            points,
            warmup_seconds=0,
            budget_mib_per_hour=0.5,
            anon_budget_mib_per_hour=0.5,
            min_span_seconds=3600,
            last_pressure_end=None,
            pressure_settle_seconds=0,
        )
        self.assertTrue(result["ok"], result)

    def test_empty_samples_fail(self):
        result = soak.analyze(
            [],
            warmup_seconds=0,
            budget_mib_per_hour=16,
            min_span_seconds=60,
            last_pressure_end=None,
            pressure_settle_seconds=0,
        )
        self.assertFalse(result["ok"])


class MeasurementWindowTests(unittest.TestCase):
    def test_pressure_must_end_before_measurement_starts(self):
        with self.assertRaises(soak.SoakError) as caught:
            soak._measurement_start(
                started=0.0,
                warmup_seconds=90.0,
                pressure_end=95.0,
                handshake_end=90.0,
            )
        self.assertIn("pressure block ended after", str(caught.exception))

        # Fast lane: the handshake is later than the warm-up floor.
        self.assertEqual(
            soak._measurement_start(0.0, 90.0, 90.0, 125.0), 125.0
        )
        # Long lane: the configured warm-up still wins when pressure/handshake
        # finish early inside it.
        self.assertEqual(
            soak._measurement_start(0.0, 1800.0, 1500.0, 1600.0), 1800.0
        )

    def test_analysis_uses_only_the_measurement_window(self):
        # 100 s of growth, then flat. Measurement starts at 100 s, so the
        # pre-measurement ramp must not contaminate the gate.
        samples = [(float(i), 100 * MIB + i * 10 * MIB) for i in range(100)]
        samples += [(float(i), 1100 * MIB) for i in range(100, 200)]
        result = soak.analyze(
            samples,
            warmup_seconds=0,
            budget_mib_per_hour=4,
            min_span_seconds=60,
            last_pressure_end=None,
            pressure_settle_seconds=0,
            measurement_start=100.0,
        )
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["label"], "RSS")
        self.assertEqual(result["effective_warmup_seconds"], 100.0)
        self.assertEqual(result["samples"], 100)
        self.assertLess(result["upper95_mib_per_hour"], 4)

    def test_measurement_window_can_still_fail_anon_independently(self):
        rss = [(float(i), 100 * MIB) for i in range(200)]
        anon = [
            (float(i), 50 * MIB + i * 10 * 4.5 * MIB / 3600.0)
            for i in range(200)
        ]
        result = soak.analyze_pair(
            rss,
            anon,
            warmup_seconds=0,
            budget_mib_per_hour=0.5,
            anon_budget_mib_per_hour=0.5,
            min_span_seconds=60,
            last_pressure_end=None,
            pressure_settle_seconds=0,
            measurement_start=10.0,
        )
        self.assertFalse(result["ok"])
        self.assertTrue(result["rss"]["ok"], result["rss"])
        self.assertFalse(result["anon"]["ok"], result["anon"])
        self.assertIn("Anon slope", result["anon"]["error"])


class CleanLoadHandshakeTests(unittest.TestCase):
    class Clock:
        def __init__(self):
            self.now = 0.0

        def monotonic(self):
            return self.now

        def sleep(self, seconds):
            self.now += seconds

    class Runner:
        """Counter snapshot source with an injectable error burst."""

        def __init__(self, *, error_calls=(), requests_per_call=8):
            self.calls = 0
            self.errors = 0
            self.requests_ok = 0
            self.error_calls = set(error_calls)
            self.requests_per_call = requests_per_call

        def summary(self):
            self.calls += 1
            self.requests_ok += self.requests_per_call
            if self.calls in self.error_calls:
                self.errors += 1
            return {
                "requests_ok": self.requests_ok,
                "errors": self.errors,
            }

    def run_handshake(self, runner, *, min_clean=10.0, timeout=60.0):
        clock = self.Clock()
        return (
            soak._wait_clean_load(
                runner,
                min_clean_seconds=min_clean,
                timeout_seconds=timeout,
                poll_interval=0.5,
                clock=clock.monotonic,
                sleep=clock.sleep,
            ),
            clock,
        )

    def test_requires_a_full_clean_window(self):
        result, clock = self.run_handshake(self.Runner())
        self.assertGreaterEqual(result["seconds"], 10.0)
        self.assertEqual(result["errors"], 0)
        self.assertGreater(result["requests_ok"], 0)
        self.assertEqual(clock.now, 10.0)

    def test_an_error_restarts_the_clean_window(self):
        # Error on the 10th poll (~5 s); the handshake must not return until a
        # full 10 s after it.
        result, clock = self.run_handshake(self.Runner(error_calls={11}))
        self.assertEqual(result["errors"], 1)
        self.assertGreaterEqual(result["seconds"], 10.0)
        self.assertEqual(clock.now, 15.0)

    def test_never_clean_traffic_is_a_bounded_failure(self):
        with self.assertRaises(soak.SoakError) as caught:
            self.run_handshake(
                self.Runner(error_calls=set(range(1, 200))),
                timeout=20.0,
            )
        self.assertIn("clean 2xx for 10s within 20s", str(caught.exception))

    def test_clean_window_below_ten_seconds_is_rejected(self):
        with self.assertRaises(soak.SoakError) as caught:
            soak._wait_clean_load(
                self.Runner(),
                min_clean_seconds=9.0,
                timeout_seconds=60.0,
            )
        self.assertIn("at least 10s", str(caught.exception))


class StabilizationTests(unittest.TestCase):
    """The post-pressure RSS refill ramp must flatten before measurement."""

    class Clock:
        def __init__(self):
            self.now = 0.0

        def monotonic(self):
            return self.now

        def sleep(self, seconds):
            self.now += seconds

    class Sample:
        def __init__(self, rss_bytes):
            self.rss_bytes = rss_bytes

    def run_wait(self, rss_at, *, timeout=180.0, max_rate=8.0, window=30.0):
        clock = self.Clock()
        captured = []

        def capture():
            return self.Sample(rss_at(clock.now))

        result = soak._wait_stabilization(
            capture,
            window_seconds=window,
            max_rate_mib_per_min=max_rate,
            required_windows=2,
            timeout_seconds=timeout,
            poll_interval=1.0,
            clock=clock.monotonic,
            sleep=clock.sleep,
            on_sample=captured.append,
        )
        return result, clock, captured

    def test_refill_ramp_then_flat_plateaus_and_excludes_the_ramp(self):
        base = 300

        def rss_at(now):
            # 120 MiB/min for 20 s (40 MiB refill), then flat.
            ramp = min(now, 20.0) * 120.0 * MIB / 60.0
            return int(base * MIB + ramp)

        result, clock, captured = self.run_wait(rss_at)
        self.assertTrue(result["plateaued"])
        # The criterion is the trailing rate, so the shallow tail of the ramp
        # already qualifies (the last ~2 MiB of a 40 MiB refill is below
        # 8 MiB/min over 30 s). Two consecutive windows are still required.
        self.assertGreaterEqual(result["seconds"], 45.0)
        self.assertLess(result["seconds"], 60.0)
        self.assertLessEqual(abs(result["plateau_rate_mib_per_min"]), 8.0)
        self.assertEqual(len(captured), result["samples"])
        self.assertEqual(result["windows"], 2)
        self.assertEqual(clock.now, result["end"])

    def test_continuous_slow_drift_still_plateaus(self):
        base = 300
        drift = 1.0  # MiB/min, below the 8 MiB/min plateau limit
        result, _, _ = self.run_wait(
            lambda now: int(base * MIB + now * drift * MIB / 60.0)
        )
        self.assertTrue(result["plateaued"])
        self.assertLessEqual(result["plateau_rate_mib_per_min"], 8.0)

    def test_ramp_that_never_flattens_is_a_bounded_violation(self):
        base = 300
        growth = 60.0  # MiB/min, far above the plateau limit

        def rss_at(now):
            return int(base * MIB + now * growth * MIB / 60.0)

        result, clock, _ = self.run_wait(rss_at, timeout=180.0)
        self.assertFalse(result["plateaued"])
        self.assertEqual(result["seconds"], 180.0)
        self.assertEqual(clock.now, 180.0)
        self.assertGreater(result["plateau_rate_mib_per_min"], 8.0)

    def test_growth_resets_the_consecutive_window_count(self):
        # A qualifying window at t=30 is followed immediately by a jump that
        # makes the next window fail. Without the reset the second evaluation
        # would wrongly be counted as a plateau and return at 31 s.
        base = 300
        jump_at = 30.5
        jump = 20.0 * MIB

        def rss_at(now):
            return int(base * MIB + (jump if now >= jump_at else 0.0))

        result, _, _ = self.run_wait(rss_at)
        self.assertTrue(result["plateaued"])
        # The jump only leaves the trailing 30 s window at ~61 s.
        self.assertGreaterEqual(result["seconds"], 60.0)
        self.assertLess(result["seconds"], 65.0)

    def test_stabilization_moves_measurement_start_and_keeps_ordering(self):
        self.assertEqual(
            soak._measurement_start(0.0, 90.0, 120.0, 135.0, 200.0), 200.0
        )
        # The long-lane warm-up floor still wins over an early plateau.
        self.assertEqual(
            soak._measurement_start(0.0, 1800.0, 120.0, 135.0, 200.0), 1800.0
        )
        with self.assertRaises(soak.SoakError):
            soak._measurement_start(0.0, 0.0, 250.0, 135.0, 200.0)


class StatusReadinessTests(unittest.TestCase):
    class Clock:
        def __init__(self):
            self.now = 0.0

        def monotonic(self):
            return self.now

        def sleep(self, seconds):
            self.now += seconds

    def run_wait(self, load, timeout=60.0):
        clock = self.Clock()
        with mock.patch.object(soak.status, "load", side_effect=load):
            result = soak._wait_status_ready(
                "status.json",
                ("workerReclaim.generation",),
                timeout,
                poll_interval=0.5,
                clock=clock.monotonic,
                sleep=clock.sleep,
            )
        return result, clock

    def test_startup_absence_is_retried_until_the_governor_appears(self):
        calls = []

        def load(source, required):
            calls.append((source, required))
            if len(calls) < 3:
                raise status.StatusError(
                    "node status has no resourceGovernor object"
                )
            return {"workerReclaim.generation": 0}

        result, clock = self.run_wait(load)
        self.assertEqual(result["workerReclaim.generation"], 0)
        self.assertEqual(len(calls), 3)
        self.assertLessEqual(clock.now, 1.0)

    def test_permanent_absence_fails_within_the_bound(self):
        def load(source, required):
            raise status.StatusError("node status has no resourceGovernor object")

        with self.assertRaises(status.StatusError) as caught:
            self.run_wait(load, timeout=2.0)
        self.assertIn("did not become available within 2s", str(caught.exception))
        self.assertIn("no resourceGovernor object", str(caught.exception))


def synthetic_series(
    *,
    growth_mib_per_hour: float = 0.0,
    pressure_window: tuple[float, float] = (72.0, 972.0),
    base_mib: float = 200.0,
    duration: float = 14400.0,
    interval: float = 2.0,
):
    """Deterministic 4 h / 2 s series with a pressure-blocked gap.

    Timestamps are real sample times: no sample exists inside the pressure
    window because the synchronous pressure cycles block the sampling loop.
    """
    points = []
    t = 0.0
    while t <= duration:
        if not (pressure_window[0] <= t < pressure_window[1]):
            growth = growth_mib_per_hour * MIB / 3600.0 * t
            points.append((t, int(base_mib * MIB + growth)))
        t += interval
    return points


class NightlyWindowContractTests(unittest.TestCase):
    """The exact 4 h nightly shape must pass its own assertions."""

    def analyze(self, rss_growth=0.0, anon_growth=0.0, pressure=(72.0, 972.0)):
        rss = synthetic_series(growth_mib_per_hour=rss_growth, pressure_window=pressure)
        anon = synthetic_series(
            growth_mib_per_hour=anon_growth, pressure_window=pressure, base_mib=100.0
        )
        blocked = pressure[1] - pressure[0]
        result = soak.analyze_pair(
            rss,
            anon,
            warmup_seconds=1800,
            budget_mib_per_hour=0.5,
            anon_budget_mib_per_hour=0.5,
            min_span_seconds=10800,
            last_pressure_end=pressure[1],
            pressure_settle_seconds=60,
        )
        span = rss[-1][0] - rss[0][0]
        expected = int(max(0.0, span - blocked) / 2.0) + 1
        return result, len(rss), expected

    def test_4h_2s_with_900s_pressure_window_passes(self):
        result, samples, expected = self.analyze()
        self.assertTrue(result["ok"], result)
        self.assertEqual(samples, expected)
        self.assertGreaterEqual(samples, expected * 0.9)
        self.assertGreaterEqual(result["rss"]["samples"], 5000)
        self.assertGreaterEqual(result["rss"]["span_seconds"], 10800)
        self.assertGreaterEqual(result["rss"]["effective_warmup_seconds"], 1800)
        for name in ("rss", "anon"):
            self.assertLess(result[name]["upper95_mib_per_hour"], 0.5)

    def test_worst_case_pressure_overrun_still_meets_the_window(self):
        # 10 cycles can block up to ~45 min if every enter/settle hits its
        # timeout. The scheduled start (1% of 4 h) keeps the remaining
        # measurable window above the 3 h minimum.
        result, samples, expected = self.analyze(pressure=(72.0, 2832.0))
        self.assertTrue(result["ok"], result)
        self.assertEqual(samples, expected)
        self.assertGreaterEqual(result["rss"]["samples"], 5000)
        self.assertGreaterEqual(result["rss"]["span_seconds"], 10800)
        # Allocator stats are read on the same loop iterations, so the stats
        # sample count equals the loop sample count; 5000 is satisfiable even
        # in this worst-case pressure window.
        self.assertGreaterEqual(samples, 5000)

    def test_long_lane_measurement_window_excludes_pressure_and_warmup(self):
        # The new correctness path: the pressure block and the configured
        # warm-up are outside the selected measurement window; no post-hoc
        # last_pressure_end exclusion is needed.
        pressure = (72.0, 972.0)
        rss = synthetic_series(pressure_window=pressure)
        anon = synthetic_series(pressure_window=pressure, base_mib=100.0)
        result = soak.analyze_pair(
            rss,
            anon,
            warmup_seconds=0,
            budget_mib_per_hour=0.5,
            anon_budget_mib_per_hour=0.5,
            min_span_seconds=10800,
            last_pressure_end=None,
            pressure_settle_seconds=0,
            measurement_start=1800.0,
        )
        self.assertTrue(result["ok"], result)
        self.assertGreaterEqual(result["rss"]["effective_warmup_seconds"], 1800)
        self.assertGreaterEqual(result["rss"]["span_seconds"], 10800)
        self.assertEqual(result["rss"]["measurement_start"], 1800.0)

    def test_4_5_mib_per_hour_still_fails_the_same_contract(self):
        result, _, _ = self.analyze(rss_growth=4.5)
        self.assertFalse(result["ok"])
        self.assertFalse(result["rss"]["ok"])
        self.assertIn("0.500 MiB/h", result["rss"]["error"])

    def test_4_5_mib_per_hour_in_anon_only_still_fails(self):
        result, _, _ = self.analyze(anon_growth=4.5)
        self.assertFalse(result["ok"])
        self.assertTrue(result["rss"]["ok"])
        self.assertFalse(result["anon"]["ok"])
        self.assertIn("0.500 MiB/h", result["anon"]["error"])


class NodeStatusTests(unittest.TestCase):
    def last(self, covers=False, generation=2, cycles=3, unhooked=8):
        return {
            "workerReclaim.generation": generation,
            "workerReclaim.unhookedPingoraWorkers": unhooked,
            "workerReclaim.coversAllWorkers": covers,
            "cacheJanitor.cycles": cycles,
        }

    def test_status_source_defaults_to_the_full_strict_field_contract(self):
        fields = soak._resolve_status_fields("http://127.0.0.1:1/status", None)
        self.assertEqual(fields, status.DEFAULT_REQUIRED_FIELDS)

    def test_explicit_fields_are_parsed_and_no_source_means_no_fields(self):
        self.assertEqual(soak._resolve_status_fields("http://x", "a.b,c"), ("a.b", "c"))
        self.assertEqual(soak._resolve_status_fields(None, "a.b"), ())

    def test_coverage_gap_is_reported_not_skipped(self):
        violations, gap = soak.node_status_outcome(self.last(covers=False), 20, 10)
        self.assertEqual(violations, [])
        self.assertIsNotNone(gap)
        self.assertEqual(gap["field"], "workerReclaim.coversAllWorkers")
        self.assertFalse(gap["value"])
        self.assertEqual(gap["unhookedPingoraWorkers"], 8)

    def test_full_worker_coverage_produces_no_gap(self):
        violations, gap = soak.node_status_outcome(
            self.last(covers=True, unhooked=0), 20, 10
        )
        self.assertEqual(violations, [])
        self.assertIsNone(gap)

    def test_missing_status_is_a_violation(self):
        violations, gap = soak.node_status_outcome(None, 0, 10)
        self.assertEqual(len(violations), 1)
        self.assertIsNone(gap)

    def test_low_samples_is_a_violation_but_zero_generation_is_not(self):
        # Presence is checked per sample; generation may legitimately be 0
        # when no worker reclaim has occurred, so it is not a nonzero gate.
        violations, _ = soak.node_status_outcome(self.last(generation=0), 5, 10)
        self.assertEqual(len(violations), 1)
        self.assertIn("samples", violations[0])
        violations, _ = soak.node_status_outcome(self.last(generation=0, cycles=0), 20, 10)
        self.assertEqual(violations, [])


class ParserTests(unittest.TestCase):
    def base_args(self):
        return [
            "--target-cmd",
            "target/debug/bench-defense",
            "--target-kind",
            "bench-defense",
            "--tls-port",
            "8443",
            "--tls-ca",
            "/tmp/ca.pem",
            "--duration",
            "600",
            "--out",
            "/tmp/report.json",
        ]

    def test_parser_accepts_required_flags(self):
        args = soak.build_parser().parse_args(self.base_args())
        self.assertEqual(args.target_kind, "bench-defense")
        self.assertEqual(args.duration_seconds, 600.0)
        self.assertEqual(args.stats_ready_timeout_seconds, 30.0)
        self.assertEqual(args.measurement_clean_seconds, 10.0)
        self.assertEqual(args.measurement_handshake_timeout_seconds, 60.0)
        self.assertEqual(args.measurement_stabilize_timeout_seconds, 180.0)
        self.assertEqual(args.status_ready_timeout_seconds, 60.0)

    def test_negative_stabilization_timeout_is_rejected(self):
        args = soak.build_parser().parse_args(
            self.base_args() + ["--measurement-stabilize-timeout-seconds", "-1"]
        )
        with self.assertRaises(soak.SoakError) as caught:
            soak.run(args)
        self.assertIn("--measurement-stabilize-timeout-seconds", str(caught.exception))

    def test_negative_status_readiness_timeout_is_rejected(self):
        args = soak.build_parser().parse_args(
            self.base_args() + ["--status-ready-timeout-seconds", "-1"]
        )
        with self.assertRaises(soak.SoakError) as caught:
            soak.run(args)
        self.assertIn("--status-ready-timeout-seconds", str(caught.exception))

    def test_measurement_clean_window_below_ten_is_rejected(self):
        args = soak.build_parser().parse_args(
            self.base_args() + ["--measurement-clean-seconds", "9"]
        )
        with self.assertRaises(soak.SoakError) as caught:
            soak.run(args)
        self.assertIn("--measurement-clean-seconds", str(caught.exception))

    def test_negative_stats_readiness_timeout_is_rejected(self):
        args = soak.build_parser().parse_args(
            self.base_args() + ["--stats-ready-timeout-seconds", "-1"]
        )
        with self.assertRaises(soak.SoakError) as caught:
            soak.run(args)
        self.assertIn("--stats-ready-timeout-seconds", str(caught.exception))

    def test_parser_requires_target_and_out(self):
        parser = soak.build_parser()
        with self.assertRaises(SystemExit):
            parser.parse_args(["--tls-port", "8443", "--tls-ca", "/tmp/ca.pem", "--duration", "1"])

    def test_main_rejects_invalid_fraction(self):
        argv = self.base_args() + ["--pressure-cycles", "1", "--pressure-start-fraction", "1.0"]
        with self.assertRaises(SystemExit):
            soak.main(argv)


if __name__ == "__main__":
    unittest.main()
