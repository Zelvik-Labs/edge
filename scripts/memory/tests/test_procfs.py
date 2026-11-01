import os
import sys
import unittest
from unittest import mock

from scripts.memory import procfs


def require_linux() -> None:
    if not sys.platform.startswith("linux"):
        if os.environ.get("ISSUE34_REQUIRE_LINUX") == "1":
            raise AssertionError("Issue 34 live /proc tests require Linux")
        raise unittest.SkipTest("live /proc tests require Linux")


VALID_STATUS = """\
Name:\tzelvik-edge
VmRSS:\t  2048 kB
RssAnon:\t  1024 kB
VmSwap:\t     0 kB
Threads:\t8
"""

VALID_ROLLUP = """\
Rss:              2048 kB
Pss:              1024 kB
Pss_Anon:          512 kB
Anonymous:        1536 kB
AnonHugePages:       0 kB
Swap:                0 kB
"""


class StatusParserTests(unittest.TestCase):
    def test_valid_status_converts_kib_to_bytes(self):
        parsed = procfs.parse_proc_status(VALID_STATUS)
        self.assertEqual(parsed["rss_bytes"], 2048 * 1024)
        self.assertEqual(parsed["anon_bytes"], 1024 * 1024)
        self.assertEqual(parsed["swap_bytes"], 0)
        self.assertEqual(parsed["threads"], 8)

    def test_missing_swap_is_an_error_not_zero(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_proc_status(VALID_STATUS.replace("VmSwap:\t     0 kB\n", ""))

    def test_missing_rss_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_proc_status(VALID_STATUS.replace("VmRSS:\t  2048 kB\n", ""))

    def test_non_numeric_value_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_proc_status(VALID_STATUS.replace("2048 kB", "many kB"))

    def test_unexpected_unit_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_proc_status(VALID_STATUS.replace("2048 kB", "2048 MB"))

    def test_negative_value_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_proc_status(VALID_STATUS.replace("2048 kB", "-1 kB"))


class RollupParserTests(unittest.TestCase):
    def test_valid_rollup(self):
        parsed = procfs.parse_smaps_rollup(VALID_ROLLUP)
        self.assertEqual(parsed["pss_bytes"], 1024 * 1024)
        self.assertEqual(parsed["anon_huge_bytes"], 0)

    def test_missing_thp_field_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_smaps_rollup(
                VALID_ROLLUP.replace("AnonHugePages:       0 kB\n", "")
            )

    def test_missing_pss_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_smaps_rollup(VALID_ROLLUP.replace("Pss:              1024 kB\n", ""))


class StatParserTests(unittest.TestCase):
    def test_starttime_is_field_22_after_parenthesised_comm(self):
        fields = ["S"] + [str(i) for i in range(1, 50)]
        fields[19] = "424242"
        text = "123 (a b) c) " + " ".join(fields) + "\n"
        self.assertEqual(procfs.parse_proc_stat(text)["starttime"], 424242)

    def test_short_stat_line_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_proc_stat("123 (comm) S 1 2 3\n")

    def test_missing_parenthesis_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_proc_stat("garbage")


class CgroupParserTests(unittest.TestCase):
    def test_events_require_all_counters(self):
        parsed = procfs.parse_cgroup_events(
            "low 0\nhigh 3\nmax 1\noom 0\noom_kill 0\n"
        )
        self.assertEqual(parsed["high"], 3)
        with self.assertRaises(procfs.StatError):
            procfs.parse_cgroup_events("high 3\nmax 1\noom 0\noom_kill 0\n")

    def test_events_zero_is_preserved(self):
        parsed = procfs.parse_cgroup_events(
            "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n"
        )
        self.assertEqual(parsed["oom_kill"], 0)

    def test_events_negative_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_cgroup_events(
                "low 0\nhigh -1\nmax 0\noom 0\noom_kill 0\n"
            )

    def test_stat_requires_anon_and_file(self):
        parsed = procfs.parse_cgroup_stat("anon 1\nfile 2\nkernel 3\nshmem 4\n")
        self.assertEqual(parsed["anon"], 1)
        with self.assertRaises(procfs.StatError):
            procfs.parse_cgroup_stat("anon 1\nfile 2\nkernel 3\n")

    def test_pressure_requires_some_line(self):
        parsed = procfs.parse_pressure(
            "some avg10=1.00 avg60=2.00 avg300=3.00 total=4\n"
            "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
        )
        self.assertEqual(parsed["some"]["avg10"], 1.0)
        with self.assertRaises(procfs.StatError):
            procfs.parse_pressure("full avg10=0 avg60=0 avg300=0 total=0\n")

    def test_pressure_missing_total_is_an_error(self):
        with self.assertRaises(procfs.StatError):
            procfs.parse_pressure("some avg10=1 avg60=2 avg300=3\n")


class SampleContractTests(unittest.TestCase):
    def test_cgroup_reader_fields_match_the_sample_constructor(self):
        contents = {
            "/test/memory.current": "100",
            "/test/memory.max": "max",
            "/test/memory.high": "max",
            "/test/memory.swap.current": "0",
            "/test/memory.swap.max": "0",
            "/test/memory.events": "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n",
            "/test/memory.stat": "anon 1\nfile 2\nkernel 3\nshmem 4\n",
            "/test/memory.pressure": "some avg10=0 avg60=0 avg300=0 total=0\n",
        }
        with mock.patch.object(procfs, "_cgroup2_dir", return_value="/test"), mock.patch.object(
            procfs, "_read_text", side_effect=contents.__getitem__
        ):
            fields = procfs.read_cgroup(123)
        accepted = set(procfs.Sample.__dataclass_fields__)
        self.assertFalse(set(fields) - accepted, "cgroup fields cannot construct a sample")
        self.assertEqual(fields["cgroup_events"]["low"], 0)


class LiveLinuxTests(unittest.TestCase):
    def test_live_proc_and_cgroup_stats_are_strict_and_present(self):
        require_linux()
        sampler = procfs.Sampler(os.getpid())
        sample = sampler.capture()
        self.assertEqual(sample.pid, os.getpid())
        self.assertGreater(sample.starttime, 0)
        self.assertGreater(sample.rss_bytes, 0)
        self.assertGreaterEqual(sample.pss_bytes, 0)
        self.assertGreaterEqual(sample.anon_huge_bytes, 0)
        self.assertGreater(sample.threads, 0)
        self.assertGreaterEqual(sample.fds, 0)
        self.assertGreater(sample.maps_count, 0)
        self.assertGreater(sample.maps_bytes, 0)
        self.assertGreaterEqual(sample.max_open_files_soft, 0)
        self.assertIn("oom_kill", sample.cgroup_events)
        self.assertIn("some", sample.cgroup_pressure)

    def test_pid_reuse_is_detected(self):
        require_linux()
        sampler = procfs.Sampler(os.getpid())
        sampler._starttime += 1  # simulate a recycled PID with a different starttime
        with self.assertRaises(procfs.PidReuseError):
            sampler.capture()


if __name__ == "__main__":
    unittest.main()
