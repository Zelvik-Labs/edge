import unittest

from scripts.memory import soak, status

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
        self.assertIn("0.500 MiB/h", result["anon"]["error"])

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
