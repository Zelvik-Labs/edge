import io
import json
import os
import tempfile
import unittest

from scripts.memory import pressure


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

    def probe(self, samples):
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


if __name__ == "__main__":
    unittest.main()
