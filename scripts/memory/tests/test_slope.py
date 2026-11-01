import unittest

from scripts.memory import slope

MIB = 1024 * 1024


def linear(count=120, base=100 * MIB, bytes_per_second=1.0, step=1.0):
    return [(i * step, base + bytes_per_second * i * step) for i in range(count)]


class SlopeTests(unittest.TestCase):
    def test_flat_series_passes(self):
        result = slope.estimate_slope(
            linear(bytes_per_second=0.0),
            warmup_seconds=10,
            budget_mib_per_hour=16,
            min_span_seconds=30,
        )
        self.assertLess(abs(result.slope_bytes_per_second), 1e-6)
        self.assertLess(result.upper95_mib_per_hour, 16)
        self.assertEqual(result.samples, 110)

    def test_small_growth_passes(self):
        # 0.1 B/s is ~0.34 MiB/h.
        result = slope.estimate_slope(
            linear(bytes_per_second=0.1),
            warmup_seconds=10,
            budget_mib_per_hour=4,
            min_span_seconds=30,
        )
        self.assertAlmostEqual(result.slope_mib_per_hour, 0.000343, places=6)

    def test_budget_violation_raises(self):
        with self.assertRaises(slope.ThresholdError):
            slope.estimate_slope(
                linear(bytes_per_second=1024 * 1024),
                warmup_seconds=10,
                budget_mib_per_hour=60,
                min_span_seconds=30,
            )

    def test_warmup_excludes_initial_spike(self):
        spike = [(i, 1000 * MIB + i * 10 * MIB) for i in range(20)]
        flat = [(20 + i, 1200 * MIB) for i in range(80)]
        result = slope.estimate_slope(
            spike + flat,
            warmup_seconds=20,
            budget_mib_per_hour=1,
            min_span_seconds=30,
        )
        self.assertLess(result.upper95_mib_per_hour, 1)

    def test_too_few_post_warmup_samples(self):
        with self.assertRaises(slope.ThresholdError):
            slope.estimate_slope(
                [(0, 1), (1, 2)],
                warmup_seconds=0,
                budget_mib_per_hour=10,
            )

    def test_duplicate_timestamps_are_an_error(self):
        with self.assertRaises(slope.ThresholdError):
            slope.estimate_slope(
                [(1, 1), (1, 2), (1, 3)],
                warmup_seconds=0,
                budget_mib_per_hour=10,
            )

    def test_non_finite_values_are_an_error(self):
        with self.assertRaises(slope.ThresholdError):
            slope.estimate_slope(
                [(0, 1), (1, float("nan")), (2, 3)],
                warmup_seconds=0,
                budget_mib_per_hour=10,
            )

    def test_short_span_is_an_error(self):
        with self.assertRaises(slope.ThresholdError):
            slope.estimate_slope(
                linear(count=20),
                warmup_seconds=0,
                budget_mib_per_hour=10,
                min_span_seconds=300,
            )

    def test_theil_sen_resists_a_single_outlier(self):
        points = [(i, 100 * MIB) for i in range(100)]
        points[-1] = (99, 200 * MIB)
        # The median of pairwise slopes ignores one late spike; this is why the
        # gate also reports the OLS upper confidence bound.
        self.assertEqual(slope._theil_sen(points), 0.0)

    def test_theil_sen_tracks_a_sustained_step(self):
        points = [(i, 100 * MIB) for i in range(50)]
        points += [(i, 200 * MIB) for i in range(50, 100)]
        self.assertGreater(slope._theil_sen(points), 0.0)

    def test_4_5_mib_per_hour_fails_the_strict_0_5_budget(self):
        # Issue 34 tail case: a slow leak reproduced at 4.5 MiB/h. It must not
        # pass a 0.5 MiB/h budget simply because the run is long.
        bytes_per_second = 4.5 * MIB / 3600.0
        points = linear(bytes_per_second=bytes_per_second, count=400, step=10.0)
        with self.assertRaises(slope.ThresholdError) as caught:
            slope.estimate_slope(
                points,
                warmup_seconds=0,
                budget_mib_per_hour=0.5,
                min_span_seconds=3600,
            )
        self.assertIn("0.500 MiB/h", str(caught.exception))

    def test_0_4_mib_per_hour_passes_the_strict_0_5_budget(self):
        points = linear(bytes_per_second=0.4 * MIB / 3600.0, count=400, step=10.0)
        result = slope.estimate_slope(
            points,
            warmup_seconds=0,
            budget_mib_per_hour=0.5,
            min_span_seconds=3600,
        )
        self.assertLess(result.upper95_mib_per_hour, 0.5)

    def test_split_baseline_gates_the_second_half_independently(self):
        flat = [(float(i * 30), 100 * MIB) for i in range(241)]
        leak = [
            (float(241 * 30 + i * 30), 100 * MIB + i * 30 * 4.5 * MIB / 3600.0)
            for i in range(240)
        ]
        with self.assertRaises(slope.ThresholdError) as caught:
            slope.estimate_split_slope(
                flat + leak,
                warmup_seconds=0,
                budget_mib_per_hour=0.5,
                min_span_seconds=60,
            )
        self.assertIn("0.500 MiB/h", str(caught.exception))

    def test_split_baseline_allows_a_flat_run(self):
        points = [(float(i * 30), 100 * MIB) for i in range(481)]
        result = slope.estimate_split_slope(
            points,
            warmup_seconds=0,
            budget_mib_per_hour=0.5,
            min_span_seconds=60,
        )
        self.assertLess(result.step_mib, 0.5)

    def test_split_baseline_catches_a_late_step(self):
        flat = [(float(i), 100 * MIB) for i in range(200)]
        stepped = [(float(200 + i), 200 * MIB) for i in range(200)]
        with self.assertRaises(slope.ThresholdError) as caught:
            slope.estimate_split_slope(
                flat + stepped,
                warmup_seconds=0,
                budget_mib_per_hour=0.5,
                min_span_seconds=60,
            )
        self.assertIn("step", str(caught.exception).lower())

    def test_t_critical_table(self):
        self.assertAlmostEqual(slope.t_critical_95(1), 6.314)
        self.assertAlmostEqual(slope.t_critical_95(30), 1.697)
        self.assertAlmostEqual(slope.t_critical_95(10_000), 1.645)
        with self.assertRaises(slope.ThresholdError):
            slope.t_critical_95(0)


if __name__ == "__main__":
    unittest.main()
