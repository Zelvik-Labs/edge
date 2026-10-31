import json
import os
import tempfile
import unittest

from scripts.memory import stats

NATIVE = {
    "stat_version": 1,
    "process": {
        "commit_current": 4096,
        "peak_commit": 8192,
        "rss_current": 2048,
        "peak_rss": 4096,
        "page_faults": 17,
    },
    "malloc_bins": [],
}


class StatsTests(unittest.TestCase):
    def test_allocator_cache_bins_are_summarized(self):
        document = {
            "process": {
                "commit_current": 1,
                "peak_commit": 2,
                "rss_current": 3,
                "page_faults": 0,
            },
            "malloc_bins": [
                {"size": 16, "cached": 4, "committed": 8},
                {"size": 32, "cached": 6, "committed": 10, "ignored": True},
            ],
            "page_bins": [{"size": 65536, "reserved": 3}],
        }
        flat = stats.normalize(document)
        self.assertEqual(flat["malloc_bins.count"], 2)
        self.assertEqual(flat["malloc_bins.cached"], 10)
        self.assertEqual(flat["malloc_bins.committed"], 18)
        self.assertEqual(flat["page_bins.reserved"], 3)
        self.assertTrue(all(value >= 0 for value in flat.values()))

    def test_native_mimalloc_shape(self):
        flat = stats.normalize(NATIVE)
        self.assertEqual(flat["process.commit_current"], 4096.0)
        self.assertEqual(flat["process.page_faults"], 17.0)

    def test_bench_nested_v3_counters_are_preserved(self):
        document = {
            "process": NATIVE["process"],
            "mimalloc": {
                "committed": {"current": 8192, "peak": 16384, "total": 32768},
                "malloc_normal": {"current": 2048, "peak": 4096, "total": 65536},
                "pages_abandoned": {"current": 2, "peak": 4, "total": 8},
                "purged": 4096,
                "pages_reclaim_on_free": 3,
                "malloc_bins": [
                    {"current": 2, "peak": 4, "total": 10, "block_size": 16},
                    {"current": 3, "peak": 5, "total": 20, "block_size": 32},
                ],
                "page_bins": [],
            },
        }
        flat = stats.normalize(document)
        self.assertEqual(flat["process.commit_current"], 4096)
        self.assertEqual(flat["mimalloc.committed.current"], 8192)
        self.assertEqual(flat["mimalloc.malloc_normal.current"], 2048)
        self.assertEqual(flat["mimalloc.pages_abandoned.current"], 2)
        self.assertEqual(flat["mimalloc.purged"], 4096)
        self.assertEqual(flat["mimalloc.pages_reclaim_on_free"], 3)
        self.assertEqual(flat["mimalloc.malloc_bins.current"], 5)
        self.assertEqual(flat["mimalloc.malloc_bins.total"], 30)
        self.assertEqual(flat["mimalloc.page_bins.count"], 0)
        self.assertNotIn("mimalloc.malloc_bins.peak", flat)

    def test_malformed_nested_v3_counter_fails(self):
        for value in (True, "missing"):
            with self.subTest(value=value):
                document = {
                    "process": NATIVE["process"],
                    "mimalloc": {
                        "committed": {"current": value, "peak": 2, "total": 3},
                    },
                }
                with self.assertRaises(stats.StatsError):
                    stats.normalize(document)

    def test_native_signed_thread_deltas_are_preserved(self):
        document = {
            "process": NATIVE["process"],
            "mimalloc": {
                "pages_abandoned": {"current": -1, "peak": 1, "total": 73},
                "page_bins": [{"current": -1, "peak": 1, "total": 3}],
            },
        }
        flat = stats.normalize(document)
        self.assertEqual(flat["mimalloc.pages_abandoned.current"], -1)
        self.assertEqual(flat["mimalloc.page_bins.current"], -1)
        self.assertEqual(flat["process.commit_current"], 4096)

    def test_worker_collection_progress_is_preserved(self):
        document = {
            "process": NATIVE["process"],
            "worker_reclaim": {
                "started": 4, "live": 4, "park_collects": 12, "stop_collects": 0,
            },
        }
        flat = stats.normalize(document)
        self.assertEqual(flat["worker_reclaim.started"], 4)
        self.assertEqual(flat["worker_reclaim.park_collects"], 12)
        self.assertEqual(flat["worker_reclaim.stop_collects"], 0)

    def test_flat_allocator_shape(self):
        flat = stats.normalize(
            {
                "allocator": {
                    "commit_current": 1,
                    "peak_commit": 2,
                    "rss_current": 3,
                    "page_faults": 4,
                }
            }
        )
        self.assertEqual(flat["process.commit_current"], 1.0)

    def test_missing_field_is_an_error(self):
        broken = json.loads(json.dumps(NATIVE))
        del broken["process"]["commit_current"]
        with self.assertRaises(stats.StatsError):
            stats.normalize(broken)

    def test_zero_present_is_accepted(self):
        zero = json.loads(json.dumps(NATIVE))
        zero["process"]["page_faults"] = 0
        self.assertEqual(stats.normalize(zero)["process.page_faults"], 0.0)

    def test_negative_is_an_error(self):
        negative = json.loads(json.dumps(NATIVE))
        negative["process"]["commit_current"] = -1
        with self.assertRaises(stats.StatsError):
            stats.normalize(negative)

    def test_non_numeric_is_an_error(self):
        bad = json.loads(json.dumps(NATIVE))
        bad["process"]["commit_current"] = "lots"
        with self.assertRaises(stats.StatsError):
            stats.normalize(bad)

    def test_file_ignores_partial_trailing_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "stats.jsonl")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(json.dumps(NATIVE) + "\n")
                handle.write('{"process": {"commit_cur')
            flat = stats.read(path)
            self.assertEqual(flat["process.commit_current"], 4096.0)

    def test_empty_file_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "empty.json")
            open(path, "w", encoding="utf-8").close()
            with self.assertRaises(stats.StatsError):
                stats.read(path)

    def test_missing_file_is_an_error(self):
        with self.assertRaises(stats.StatsError):
            stats.read("/nonexistent/issue34/stats.json")

    def test_required_keys_parser(self):
        self.assertEqual(stats.parse_required_keys("a.b, c"), ("a.b", "c"))
        with self.assertRaises(stats.StatsError):
            stats.parse_required_keys(" , ")


if __name__ == "__main__":
    unittest.main()
