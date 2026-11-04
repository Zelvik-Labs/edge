import argparse
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest

import summarize_architecture_candidates as summary


class SummaryTests(unittest.TestCase):
    def run_cases(self, cases):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / 'manifest.json'
            manifest.write_text(json.dumps({
                'status': 'completed', 'target': 'edge', 'args': {'payload_size': 1024},
                'driver_sha256': 'driver', 'client_sha256': 'client', 'images': {},
                'cases': cases}))
            index = root / 'index.json'
            index.write_text(json.dumps({'status': 'completed', 'runs': [{
                'label': 'candidate', 'repeat': 0, 'manifest': manifest.name, 'returncode': 0}]}))
            with contextlib.redirect_stdout(io.StringIO()):
                summary.main(argparse.Namespace(index=index, output=root / 'evidence'))
            result = json.loads((root / 'evidence/results.json').read_text())
            csv = (root / 'evidence/cases.csv').read_text().splitlines()
            self.assertEqual(len(csv), len(cases) + 1)
            return result

    @staticmethod
    def case(status, native=None):
        row = {'protocol': 'h1', 'cache': 'off', 'connection': 'reuse',
               'stem': 'case', 'fresh_instance': True, 'status': status}
        if native is not None:
            row['native'] = native
        return row

    def test_medians_and_failed_transfers_are_both_retained(self):
        result = self.run_cases([
            self.case('passed', {'good_rps': 100, 'successful': 100, 'failed': 0, 'p99_ms': 2}),
            self.case('traffic-errors', {'good_rps': 0, 'successful': 0, 'failed': 9, 'p99_ms': None}),
            self.case('passed', {'good_rps': 300, 'successful': 300, 'failed': 0, 'p99_ms': 6})])
        group = result['groups'][0]
        self.assertEqual(group['good_rps_median'], 100)
        self.assertEqual(group['p99_ms_median'], 4)
        self.assertEqual(group['passed'], 2)
        self.assertAlmostEqual(group['error_fraction'], 9 / 409)
        self.assertIn('traffic_or_state_failure', group['quality_flags'])
        self.assertEqual(result['case_count'], 3)

    def test_incomplete_native_batch_has_unknown_error_fraction(self):
        case = self.case('failed')
        case['elapsed'] = 2
        result = self.run_cases([case])
        self.assertIsNone(result['groups'][0]['error_fraction'])
        self.assertIsNone(result['groups'][0]['good_rps_median'])
        self.assertEqual(result['groups'][0]['known_native_samples'], 0)
        self.assertIsNone(result['groups'][0]['client_cpu_fraction_of_2cores_median'])

    def test_all_failed_batch_keeps_zero_good_rps_and_full_error_rate(self):
        result = self.run_cases([self.case('traffic-errors', {
            'good_rps': 0, 'successful': 0, 'failed': 10})])
        self.assertEqual(result['groups'][0]['good_rps_median'], 0)
        self.assertEqual(result['groups'][0]['error_fraction'], 1)


if __name__ == '__main__':
    unittest.main()
