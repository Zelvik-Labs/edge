import io
import json
import os
import tempfile
import unittest
from unittest import mock

from scripts.memory import pressure


class FakeSample:
    def __init__(self, current, high, max_bytes, high_events=0, swap=0):
        self.cgroup_current_bytes = current
        self.cgroup_high_bytes = high
        self.cgroup_max_bytes = max_bytes
        self.cgroup_swap_current_bytes = swap
        self.cgroup_events = {
            "low": 0,
            "high": high_events,
            "max": 0,
            "oom": 0,
            "oom_kill": 0,
        }
        self.rss_bytes = current
        self.starttime = 77

    def to_dict(self):
        return {
            "cgroup_current_bytes": self.cgroup_current_bytes,
            "cgroup_high_bytes": self.cgroup_high_bytes,
            "cgroup_max_bytes": self.cgroup_max_bytes,
            "cgroup_swap_current_bytes": self.cgroup_swap_current_bytes,
            "cgroup_events": dict(self.cgroup_events),
            "rss_bytes": self.rss_bytes,
            "starttime": self.starttime,
        }


class FakeSampler:
    pid = 1234
    starttime = 77

    def __init__(self, samples):
        self.samples = list(samples)
        self.index = 0

    def capture(self):
        sample = self.samples[min(self.index, len(self.samples) - 1)]
        self.index += 1
        return sample

    def verify_identity(self):
        pass


class PressureParsingTests(unittest.TestCase):
    def test_int_or_max(self):
        self.assertIsNone(pressure._int_or_max("max\n", "memory.max"))
        self.assertEqual(pressure._int_or_max("0\n", "memory.swap.max"), 0)
        self.assertEqual(pressure._int_or_max("2147483648\n", "memory.max"), 2**31)
        with self.assertRaises(pressure.PressureError):
            pressure._int_or_max("wat\n", "memory.max")
        with self.assertRaises(pressure.PressureError):
            pressure._int_or_max("-1\n", "memory.max")

    def test_progress_emits_parseable_json(self):
        stream = io.StringIO()
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "progress.jsonl")
            progress = pressure.Progress(path, stream=stream)
            record = progress.emit("cycle_start", cycle=1, pid=42)
            self.assertEqual(record["phase"], "cycle_start")
            self.assertEqual(record["cycle"], 1)
            line = stream.getvalue().strip()
            self.assertTrue(line.startswith(pressure.PROGRESS_PREFIX))
            parsed = json.loads(line[len(pressure.PROGRESS_PREFIX):])
            self.assertEqual(parsed["pid"], 42)
            with open(path, encoding="utf-8") as handle:
                self.assertIn("ISSUE34_PRESSURE", handle.read())

    def test_progress_requires_all_record_fields(self):
        stream = io.StringIO()
        progress = pressure.Progress(stream=stream)
        progress.emit("recovered", same_pid=True)
        parsed = json.loads(stream.getvalue().split(" ", 1)[1])
        self.assertTrue(parsed["same_pid"])


class PressureSizingTests(unittest.TestCase):
    """The CI failure was a fixed 1.5 GiB allocation under high=1.5 GiB."""

    MIB = pressure.MIB

    def size(self, *, current, high, max_bytes, requested):
        return pressure._cycle_pressure_size(
            current_bytes=current,
            high_bytes=high,
            max_bytes=max_bytes,
            requested_bytes=requested,
        )

    def test_failed_ci_setup_targets_bounded_over_high(self):
        # max=2 GiB, high=1.5 GiB, current=384 MiB, requested=1.5 GiB.
        alloc, target = self.size(
            current=384 * self.MIB,
            high=1536 * self.MIB,
            max_bytes=2048 * self.MIB,
            requested=1536 * self.MIB,
        )
        self.assertEqual(target, 1552 * self.MIB)
        self.assertEqual(alloc, 1168 * self.MIB)
        # Crosses high by the bounded margin, keeps 256 MiB below max.
        self.assertEqual(target - 1536 * self.MIB, 16 * self.MIB)
        self.assertEqual(2048 * self.MIB - target, 496 * self.MIB)
        self.assertLessEqual(target, 2048 * self.MIB - pressure.PRESSURE_RESERVE_BYTES)

    def test_request_below_the_pressure_need_is_rejected(self):
        with self.assertRaises(pressure.PressureError) as caught:
            self.size(
                current=384 * self.MIB,
                high=1536 * self.MIB,
                max_bytes=2048 * self.MIB,
                requested=256 * self.MIB,
            )
        self.assertIn("below the", str(caught.exception))
        self.assertIn("increase --pressure-bytes", str(caught.exception))

    def test_already_above_high_allocates_only_the_minimum(self):
        alloc, target = self.size(
            current=1700 * self.MIB,
            high=1536 * self.MIB,
            max_bytes=2048 * self.MIB,
            requested=1536 * self.MIB,
        )
        self.assertEqual(alloc, pressure.MIN_PRESSURE_BYTES)
        self.assertEqual(target, 1764 * self.MIB)
        self.assertLessEqual(target, 2048 * self.MIB - pressure.PRESSURE_RESERVE_BYTES)

    def test_insufficient_headroom_is_a_clear_error(self):
        with self.assertRaises(pressure.PressureError) as caught:
            self.size(
                current=1750 * self.MIB,
                high=1536 * self.MIB,
                max_bytes=2048 * self.MIB,
                requested=1536 * self.MIB,
            )
        self.assertIn("headroom is too small", str(caught.exception))

    def test_high_too_close_to_max_is_a_clear_error(self):
        with self.assertRaises(pressure.PressureError) as caught:
            self.size(
                current=384 * self.MIB,
                high=2040 * self.MIB,
                max_bytes=2048 * self.MIB,
                requested=1536 * self.MIB,
            )
        self.assertIn("too close to memory.max", str(caught.exception))

    def test_unlimited_max_uses_the_requested_size(self):
        alloc, target = self.size(
            current=384 * self.MIB,
            high=1536 * self.MIB,
            max_bytes=None,
            requested=1536 * self.MIB,
        )
        self.assertEqual(alloc, 1536 * self.MIB)
        self.assertEqual(target, 1920 * self.MIB)

    def test_high_none_cannot_generate_events(self):
        with self.assertRaises(pressure.PressureError) as caught:
            self.size(
                current=384 * self.MIB,
                high=None,
                max_bytes=2048 * self.MIB,
                requested=1536 * self.MIB,
            )
        self.assertIn("memory.high is 'max'", str(caught.exception))


class AllocatorTests(unittest.TestCase):
    def test_allocator_signals_readiness_and_exits_cleanly(self):
        pid, ready_fd, release_fd = pressure._spawn_allocator(8 * 1024 * 1024)
        try:
            pressure._wait_ready(ready_fd, timeout=30)
        finally:
            try:
                os.close(ready_fd)
            except OSError:
                pass
            pressure._release_and_reap(pid, release_fd)

    def test_run_cycles_rejects_zero_cycles(self):
        class Dummy:
            pid = 1
            starttime = 1

        with self.assertRaises(pressure.PressureError):
            pressure.run_cycles(
                Dummy(),
                cycles=0,
                pressure_bytes=64 * 1024 * 1024,
                hold_seconds=1,
                settle_timeout_seconds=1,
                progress=pressure.Progress(stream=io.StringIO()),
            )

    def test_run_cycles_rejects_tiny_pressure(self):
        class Dummy:
            pid = 1
            starttime = 1

        with self.assertRaises(pressure.PressureError):
            pressure.run_cycles(
                Dummy(),
                cycles=1,
                pressure_bytes=1024,
                hold_seconds=1,
                settle_timeout_seconds=1,
                progress=pressure.Progress(stream=io.StringIO()),
            )


class LongRecoveryValidationTests(unittest.TestCase):
    class Dummy:
        pid = 1
        starttime = 1

    def run_cycles(self, **overrides):
        values = {
            "cycles": 10,
            "pressure_bytes": 64 * 1024 * 1024,
            "hold_seconds": 60,
            "settle_timeout_seconds": 1,
            "progress": pressure.Progress(stream=io.StringIO()),
            "recovery_probe_seconds": 30,
            "long_recovery": True,
        }
        values.update(overrides)
        return pressure.run_cycles(self.Dummy(), **values)

    def test_long_recovery_requires_ten_cycles(self):
        with self.assertRaises(pressure.PressureError) as caught:
            self.run_cycles(cycles=9)
        self.assertIn("at least 10", str(caught.exception))

    def test_long_recovery_requires_sixty_second_hold(self):
        with self.assertRaises(pressure.PressureError) as caught:
            self.run_cycles(hold_seconds=59)
        self.assertIn("hold_seconds", str(caught.exception))

    def test_long_recovery_requires_probe(self):
        with self.assertRaises(pressure.PressureError) as caught:
            self.run_cycles(recovery_probe_seconds=29)
        self.assertIn("recovery_probe_seconds", str(caught.exception))


class SustainedRecoveryProbeTests(unittest.TestCase):
    class Sample:
        def __init__(self, current, swap=0):
            self.cgroup_current_bytes = current
            self.cgroup_swap_current_bytes = swap

    class Sampler:
        pid = 7
        starttime = 11

        def __init__(self, samples):
            self.samples = list(samples)
            self.index = 0

        def capture(self):
            sample = self.samples[min(self.index, len(self.samples) - 1)]
            self.index += 1
            return sample

        def verify_identity(self):
            pass

    class Clock:
        """Fake monotonic clock so the probe covers every sample deterministically."""

        def __init__(self):
            self.now = 0.0

        def monotonic(self):
            return self.now

        def sleep(self, seconds):
            self.now += seconds

    def probe(self, samples):
        clock = self.Clock()
        with (
            mock.patch.object(pressure.time, "monotonic", clock.monotonic),
            mock.patch.object(pressure.time, "sleep", clock.sleep),
        ):
            return pressure._probe_sustained_recovery(
                self.Sampler(samples),
                ceiling_bytes=1000,
                seconds=0.02,
                poll_interval=0.001,
                progress=pressure.Progress(stream=io.StringIO()),
                cycle=1,
            )

    def test_probe_requires_a_sustained_window(self):
        samples = [self.Sample(900)] * 8
        self.assertGreaterEqual(self.probe(samples), 1)

    def test_a_late_dip_back_over_the_ceiling_fails(self):
        samples = [self.Sample(900), self.Sample(900), self.Sample(1200)]
        with self.assertRaises(pressure.PressureError) as caught:
            self.probe(samples)
        self.assertIn("not sustained", str(caught.exception))

    def test_swap_during_the_probe_fails(self):
        samples = [self.Sample(900, swap=4096)]
        with self.assertRaises(pressure.PressureError) as caught:
            self.probe(samples)
        self.assertIn("swap", str(caught.exception))


class CycleSizingIntegrationTests(unittest.TestCase):
    """Exercise run_cycles with the exact failing CI cgroup numbers."""

    MIB = pressure.MIB
    Sample = FakeSample
    Sampler = FakeSampler

    def test_failed_ci_numbers_allocate_the_capped_size(self):
        current = 384 * self.MIB
        high = 1536 * self.MIB
        max_bytes = 2048 * self.MIB
        sampler = self.Sampler(
            [
                self.Sample(current, high, max_bytes),
                self.Sample(current, high, max_bytes),
                self.Sample(1552 * self.MIB, high, max_bytes, high_events=1),
                self.Sample(420 * self.MIB, high, max_bytes, high_events=1),
                self.Sample(420 * self.MIB, high, max_bytes, high_events=1),
            ]
        )
        allocated = []
        ready_r, ready_w = os.pipe()
        release_r, release_w = os.pipe()

        def spawn(size):
            allocated.append(size)
            return 4242, ready_r, release_w

        def release(pid, fd, **kwargs):
            os.close(fd)


        with (
            mock.patch.object(pressure, "_spawn_allocator", side_effect=spawn),
            mock.patch.object(pressure, "_wait_ready"),
            mock.patch.object(pressure, "_release_and_reap", side_effect=release),
        ):
            result = pressure.run_cycles(
                sampler,
                cycles=1,
                pressure_bytes=1536 * self.MIB,
                hold_seconds=0,
                settle_timeout_seconds=1,
                progress=pressure.Progress(stream=io.StringIO()),
                recovery_probe_seconds=0,
            )
        os.close(ready_w)
        os.close(release_r)
        self.assertEqual(allocated, [1168 * self.MIB])
        self.assertEqual(result["allocated_bytes"], [1168 * self.MIB])
        self.assertEqual(result["target_bytes"], [1552 * self.MIB])
        self.assertEqual(result["event_deltas"]["high"], 1)
        self.assertTrue(result["same_pid"])

    def test_request_below_need_fails_before_spawning(self):
        current = 384 * self.MIB
        high = 1536 * self.MIB
        sampler = self.Sampler(
            [
                self.Sample(current, high, 2048 * self.MIB),
                self.Sample(current, high, 2048 * self.MIB),
            ]
        )

        with mock.patch.object(pressure, "_spawn_allocator") as spawn:
            with self.assertRaises(pressure.PressureError) as caught:
                pressure.run_cycles(
                    sampler,
                    cycles=1,
                    pressure_bytes=256 * self.MIB,
                    hold_seconds=0,
                    settle_timeout_seconds=1,
                    progress=pressure.Progress(stream=io.StringIO()),
                )
        self.assertIn("increase --pressure-bytes", str(caught.exception))
        spawn.assert_not_called()

    def test_primary_resident_error_is_not_masked_by_cleanup(self):
        current = 384 * self.MIB
        high = 1536 * self.MIB
        sampler = self.Sampler(
            [
                self.Sample(current, high, 2048 * self.MIB),
                self.Sample(current, high, 2048 * self.MIB),
            ]
        )
        ready_r, ready_w = os.pipe()
        release_r, release_w = os.pipe()

        with (
            mock.patch.object(
                pressure,
                "_spawn_allocator",
                return_value=(4242, ready_r, release_w),
            ),
            mock.patch.object(
                pressure,
                "_wait_ready",
                side_effect=pressure.PressureError("resident timeout"),
            ),
            mock.patch.object(
                pressure,
                "_release_and_reap",
                side_effect=pressure.PressureError("cleanup timeout"),
            ),
        ):
            with self.assertRaises(pressure.PressureError) as caught:
                pressure.run_cycles(
                    sampler,
                    cycles=1,
                    pressure_bytes=1536 * self.MIB,
                    hold_seconds=0,
                    settle_timeout_seconds=1,
                    progress=pressure.Progress(stream=io.StringIO()),
                )
        os.close(ready_w)
        os.close(release_r)
        self.assertEqual(str(caught.exception), "resident timeout")


class ResidentProofTests(unittest.TestCase):
    """Resident proof is bounded and failures carry a cgroup snapshot."""

    MIB = pressure.MIB
    Sample = FakeSample

    def test_timeout_is_bounded_and_reports_a_snapshot(self):
        ready_r, ready_w = os.pipe()
        stream = io.StringIO()
        sampler = FakeSampler(
            [self.Sample(1500 * self.MIB, 1536 * self.MIB, 2048 * self.MIB)]
        )
        try:
            with self.assertRaises(pressure.PressureError) as caught:
                pressure._wait_ready(
                    ready_r,
                    timeout=0.3,
                    sampler=sampler,
                    cycle=1,
                    allocated_bytes=1168 * self.MIB,
                    target_bytes=1552 * self.MIB,
                    progress=pressure.Progress(stream=stream),
                    poll_interval=0.02,
                )
        finally:
            os.close(ready_r)
            os.close(ready_w)
        message = str(caught.exception)
        self.assertIn("did not make its pages resident within", message)
        self.assertIn("current=1572864000", message)
        self.assertIn("target=1627389952", message)
        self.assertGreaterEqual(sampler.index, 1)
        self.assertIn("resident_timeout", stream.getvalue())

    def test_full_edge_hold_does_not_extend_the_resident_timeout(self):
        current = 384 * self.MIB
        high = 1536 * self.MIB
        max_bytes = 2048 * self.MIB
        samples = [
            CycleSizingIntegrationTests.Sample(current, high, max_bytes),
            CycleSizingIntegrationTests.Sample(current, high, max_bytes),
            CycleSizingIntegrationTests.Sample(
                1552 * self.MIB, high, max_bytes, high_events=1
            ),
            CycleSizingIntegrationTests.Sample(
                420 * self.MIB, high, max_bytes, high_events=1
            ),
            CycleSizingIntegrationTests.Sample(
                420 * self.MIB, high, max_bytes, high_events=1
            ),
        ]
        sampler = CycleSizingIntegrationTests.Sampler(samples)
        ready_r, ready_w = os.pipe()
        release_r, release_w = os.pipe()

        def release(pid, fd, **kwargs):
            os.close(fd)

        with (
            mock.patch.object(
                pressure,
                "_spawn_allocator",
                return_value=(4242, ready_r, release_w),
            ),
            mock.patch.object(pressure, "_wait_ready") as wait_ready,
            mock.patch.object(pressure, "_release_and_reap", side_effect=release),
            mock.patch.object(pressure.time, "sleep"),
        ):
            pressure.run_cycles(
                sampler,
                cycles=1,
                pressure_bytes=1536 * self.MIB,
                hold_seconds=300,
                settle_timeout_seconds=1,
                progress=pressure.Progress(stream=io.StringIO()),
            )
        os.close(ready_w)
        os.close(release_r)
        self.assertEqual(
            wait_ready.call_args.kwargs["timeout"],
            pressure.RESIDENT_TIMEOUT_SECONDS,
        )
        self.assertLessEqual(
            pressure.RESIDENT_TIMEOUT_SECONDS,
            120.0,
        )


if __name__ == "__main__":
    unittest.main()
