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
