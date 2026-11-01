import json
import os
import tempfile
import unittest

from scripts.memory import status


def governor():
    return {
        "resourceGovernor": {
            "workerReclaim": {
                "generation": 3,
                "requests": 9,
                "started": 4,
                "live": 4,
                "stopped": 0,
                "parkCollects": 7,
                "rateLimited": 1,
                "stopCollects": 2,
                "unhookedPingoraWorkers": 0,
                "coversAllWorkers": True,
            },
            "cacheJanitor": {
                "cycles": 12,
                "maintenanceCycles": 10,
                "admissionSkips": 2,
                "budgetPublishes": 3,
                "rebuildsDeferred": 1,
                "rebuildsApplied": 2,
                "negativeEntriesRemoved": 4,
                "workerCollectRequests": 5,
            },
            "metricValuesCache": {
                "entries": 6,
                "estimatedBytes": 1024,
                "maxEntries": 100,
                "maxEstimatedBytes": 65536,
                "evictedEntries": 0,
            },
            "metricsAggregatorBytes": 2048,
        }
    }


class StatusTests(unittest.TestCase):
    def test_complete_snapshot(self):
        flat = status.normalize(governor())
        self.assertEqual(flat["workerReclaim.generation"], 3)
        self.assertTrue(flat["workerReclaim.coversAllWorkers"])
        self.assertEqual(flat["metricValuesCache.estimatedBytes"], 1024)

    def test_missing_field_is_an_error(self):
        document = governor()
        del document["resourceGovernor"]["workerReclaim"]["parkCollects"]
        with self.assertRaises(status.StatusError):
            status.normalize(document)

    def test_bool_field_requires_boolean(self):
        document = governor()
        document["resourceGovernor"]["workerReclaim"]["coversAllWorkers"] = 1
        with self.assertRaises(status.StatusError):
            status.normalize(document)

    def test_negative_is_an_error(self):
        document = governor()
        document["resourceGovernor"]["cacheJanitor"]["cycles"] = -1
        with self.assertRaises(status.StatusError):
            status.normalize(document)

    def test_aggregator_smaller_than_cache_is_an_error(self):
        document = governor()
        document["resourceGovernor"]["metricsAggregatorBytes"] = 1
        with self.assertRaises(status.StatusError):
            status.normalize(document)

    def test_status_json_wrapper(self):
        wrapped = {"status_json": json.dumps(governor())}
        flat = status.normalize(wrapped)
        self.assertEqual(flat["cacheJanitor.cycles"], 12)

    def test_camel_case_status_json_wrapper(self):
        wrapped = {"statusJson": json.dumps(governor())}
        flat = status.normalize(wrapped)
        self.assertEqual(flat["workerReclaim.generation"], 3)

    def test_reports_list_uses_last(self):
        wrapped = {"reports": [governor(), governor()]}
        flat = status.normalize(wrapped)
        self.assertEqual(flat["metricValuesCache.entries"], 6)

    def test_jsonl_file_uses_last_complete_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "status.jsonl")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(json.dumps(governor()) + "\n")
                handle.write('{"resourceGovernor": {"workerReclaim":')
            flat = status.load(path)
            self.assertEqual(flat["workerReclaim.requests"], 9)

    def test_empty_file_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "empty.json")
            open(path, "w", encoding="utf-8").close()
            with self.assertRaises(status.StatusError):
                status.load(path)

    def test_missing_file_is_an_error(self):
        with self.assertRaises(status.StatusError):
            status.load("/nonexistent/issue34/status.json")

    def test_required_fields_parser(self):
        self.assertEqual(status.parse_required_fields("a,b"), ("a", "b"))
        with self.assertRaises(status.StatusError):
            status.parse_required_fields(" , ")


if __name__ == "__main__":
    unittest.main()
