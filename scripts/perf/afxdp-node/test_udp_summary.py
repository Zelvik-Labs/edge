"""Check comparison direction, invalid-run handling and visible noisy runs."""
import copy
import unittest
from udp_summary import COMPARISON_NODES, summarize


def fixture():
    metadata = {'rounds': 4, 'cpus': [0, 1, 2, 3], 'clock_ticks': 100,
                'comparison': 'wake', 'targets': ['worker', 'readable', 'nginx', 'openresty']}
    def snapshot(ns, ticks):
        return {'monotonic_ns': ns, 'processes': {'42': {'ticks': ticks, 'fd_count': 3, 'threads': [{}]}}}
    def cpu(ns, ticks):
        return {'monotonic_ns': ns, 'proc': {'cpu'+str(i): [ticks, 0, 0, ticks, 0, 0, 0, 0, 0, 0] for i in range(4)}}
    records = []
    for concurrency in [1, 16]:
        for payload in [64, 1200]:
            for target, rps in [('direct', 1000), ('worker', 100), ('readable', 150), ('nginx', 200), ('openresty', 250)]:
                for repeat in ([0] if target == 'direct' else range(1, 5)):
                    records.append({'target': target, 'round': repeat, 'concurrency': concurrency, 'payload': payload, 'passed': True, 'errors': [],
                        'result': {'elapsed_ns': 1_000_000_000, 'requests_per_second': rps, 'p99_rtt_ns': 3_000_000/rps,
                            'timed_cpu_user_seconds': .2, 'timed_cpu_system_seconds': .1},
                        'cpu_before': cpu(0, 10), 'cpu_after': cpu(1_000_000_000, 60),
                        'server_before': snapshot(0, 20), 'server_after': snapshot(1_000_000_000, 80),
                        'origin_before': snapshot(0, 0), 'origin_after': snapshot(1_000_000_000, 10),
                        'idle_before': snapshot(1_000_000_000, 80), 'idle_after': snapshot(3_000_000_000, 80)})
    return records, metadata


class SummaryTests(unittest.TestCase):
    def test_paired_ratios_and_cpu_units(self):
        records, metadata = fixture()
        summary = summarize(records, metadata)
        self.assertTrue(summary['passed'])
        paired = summary['comparisons']['c1-b64']['paired_changes_pct']
        self.assertEqual(paired['readable_vs_worker']['requests_per_second']['all'], [50]*4)
        self.assertAlmostEqual(paired['readable_vs_worker']['p99_rtt_ns']['median'], -100/3)
        self.assertEqual(paired['worker_vs_nginx']['requests_per_second']['median'], -50)
        resource = summary['resources']['c1-b64-worker-r1']
        self.assertAlmostEqual(resource['client_cpu_pct_of_one_core'], 30)
        self.assertEqual(resource['proxy_cpu_pct_of_one_core'], 60)
        self.assertEqual(resource['idle_proxy_cpu_pct_of_one_core'], 0)
        self.assertEqual(resource['quality_flags'], [])

    def test_failed_or_missing_runs_withhold_the_cell_comparison(self):
        original, metadata = fixture()
        for incomplete in [False, True]:
            records = copy.deepcopy(original)
            if incomplete:
                records.pop(1)
            else:
                records[1]['passed'] = False
                records[1]['errors'] = ['body mismatch']
            summary = summarize(records, metadata)
            self.assertFalse(summary['passed'])
            self.assertNotIn('paired_changes_pct', summary['comparisons']['c1-b64'])
            self.assertTrue(summary['comparisons']['c16-b1200']['passed'])

    def test_cpu_and_calibration_limits_are_retained(self):
        records, metadata = fixture()
        noisy = records[1]
        noisy['result']['timed_cpu_user_seconds'] = .85
        noisy['result']['requests_per_second'] = 900
        noisy['cpu_after']['proc']['cpu0'][7] = 5
        summary = summarize(records, metadata)
        self.assertTrue(summary['passed'])
        flags = summary['resources']['c1-b64-worker-r1']['quality_flags']
        self.assertIn('client_cpu_at_least_90pct', flags)
        self.assertIn('host_cpu_steal_over_1pct', flags)
        self.assertIn('throughput_near_direct_calibration_ceiling', flags)
        self.assertIn(900, summary['comparisons']['c1-b64']['rps']['worker']['all'])

    def test_statistics_comparison_pairs_the_same_worker_mode(self):
        records, metadata = fixture()
        for record in records:
            if record['target'] in ['worker', 'readable']:
                record['target'] = {'worker': 'full-stats', 'readable': 'bridge-stats'}[record['target']]
        metadata.update(comparison='rx-stats', targets=['full-stats', 'bridge-stats', 'nginx', 'openresty'])
        summary = summarize(records, metadata)
        self.assertTrue(summary['passed'])
        pair = summary['comparisons']['c1-b64']['paired_changes_pct']['bridge-stats_vs_full-stats']
        self.assertEqual(pair['requests_per_second']['all'], [50]*4)
        self.assertNotIn('readable_vs_worker', summary['comparisons']['c1-b64']['paired_changes_pct'])
        self.assertEqual({wake for wake, _ in COMPARISON_NODES['rx-stats'].values()}, {'worker'})

    def test_mislabeled_comparison_metadata_is_rejected(self):
        records, metadata = fixture()
        metadata['comparison'] = 'rx-stats'
        with self.assertRaisesRegex(AssertionError, 'target labels'):
            summarize(records, metadata)


if __name__ == '__main__':
    unittest.main()
