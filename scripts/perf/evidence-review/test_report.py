#!/usr/bin/env python3
"""Boundary coverage for mixed replay schemas and retained failures."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('review', Path(__file__).with_name('review.py'))
review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(review)


class ReportSchemas(unittest.TestCase):
    def test_count_and_capacity_medians_can_share_report(self):
        common = dict(run=1, source='f' * 40, cells=2, successful=20, failed=0)
        counts = dict(common, label='counts', medians=[dict(target='edge', protocol='h1', total=8.4, named={'futex': 1.9})])
        capacity = dict(common, label='capacity', medians=[dict(target='pingora', protocol='h1', rps=100, cpu_usec_per_request=90, p99_ms=3)])
        report = review.render_report([counts, capacity], [])
        self.assertIn('## counts', report)
        self.assertIn('## capacity', report)
        self.assertIn('| h1 | pingora | 100.00 | 90.000 | 3.000 |', report)
        self.assertNotIn('Failed reviews', report)

    def test_failed_replay_remains_visible(self):
        failure = dict(label='cross', error_type='AssertionError', error='digest mismatch')
        report = review.render_report([], [failure])
        self.assertIn('## Failed reviews', report)
        self.assertIn('cross: AssertionError: digest mismatch', report)

    def test_caller_samples_retain_scope(self):
        row = dict(label='callers', run=1, source='f' * 40, cells=4, successful=20, failed=0,
                   groups=[dict(samples=300, groups={'Mace checkpoint': 280})])
        report = review.render_report([row], [])
        self.assertIn('Mace checkpoint', report)
        self.assertIn('do not establish delivery decimation or CPU/response percentages', report)


if __name__ == '__main__':
    unittest.main()
