#!/usr/bin/env python3
"""Corrupted raw counters/percentiles and false cache hits must fail validation."""
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('evidence', Path(__file__).parent/'http-state/verify.py')
evidence = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evidence)


class EvidenceTests(unittest.TestCase):
    def fixture(self, root):
        folder = root/'round-0-edge-1024'
        folder.mkdir()
        payload = b'x'*1024
        (folder/'payload').write_bytes(payload)
        digest = hashlib.sha256(payload).hexdigest()
        for shard in range(2):
            with gzip.open(folder/f'0000-h3-hit-fresh.{shard}.client.tsv.gz', 'wt') as file:
                file.write('200\t3\t1\t1024\t0.002\t0.001\t0\t1\n')
        integrity = {'tls_verified': True, 'protocol': '3', 'sha256': digest}
        case = {'protocol': 'h3', 'cache': 'hit', 'connection': 'fresh', 'status': 'passed', 'fresh_instance': True,
                'stem': '0000-h3-hit-fresh', 'origin_requests': 0, 'origin_counter_before': 10, 'origin_counter_after': 11,
                'native': {'successful': 2, 'failed': 0, 'request_limit': 2, 'good_rps': 20, 'p99_ms': 2,
                           'new_connections': 2, 'error_distribution': {}},
                'elapsed': .1, 'client_cpu_seconds': .05, 'proxy_cpu_usec_per_200': 200,
                'proxy_before': {'cpu.stat': {'usage_usec': 0}, 'memory.events': {'oom_kill': 0}},
                'proxy_after': {'cpu.stat': {'usage_usec': 400}, 'memory.events': {'oom_kill': 0}, 'memory.peak': {'bytes': 1<<20}},
                'origin_before': {'cpu.stat': {'usage_usec': 0}}, 'origin_after': {'cpu.stat': {'usage_usec': 0}},
                'host_cpu_before': {f'cpu{i}': [0]*8 for i in range(4)},
                'host_cpu_after': {f'cpu{i}': [1, 0, 0, 9, 0, 0, 0, 0] for i in range(4)},
                'integrity_before': integrity, 'integrity_after': integrity}
        manifest = {'status': 'completed', 'target': 'edge', 'args': {'payload_size': 1024},
                    'source_revision': 'fixture', 'payload_sha256': digest,
                    'cpu_assignment': {'proxy': [0], 'client': [1, 2], 'origin': [3]}, 'cases': [case]}
        path = folder/'manifest.json'
        path.write_text(json.dumps(manifest))
        return path, manifest

    def test_recomputes_transfers_and_cpu_without_a_server(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, _ = self.fixture(root)
            rows = evidence.validate(path, root)
            self.assertEqual(rows[0]['rps'], 20)
            self.assertEqual(rows[0]['unique_connections_per_client'], [1, 1])
            self.assertEqual(rows[0]['cpu_usec_per_request'], 200)

    def test_rejects_corrupted_percentile_and_origin_free_claim(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, manifest = self.fixture(root)
            manifest['cases'][0]['native']['p99_ms'] = 1
            path.write_text(json.dumps(manifest))
            with self.assertRaises(AssertionError):
                evidence.validate(path, root)
            manifest['cases'][0]['native']['p99_ms'] = 2
            manifest['cases'][0]['origin_requests'] = 1
            manifest['cases'][0]['origin_counter_after'] = 12
            path.write_text(json.dumps(manifest))
            with self.assertRaises(AssertionError):
                evidence.validate(path, root)

    def test_missing_transfer_output_stays_a_failed_cell(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, manifest = self.fixture(root)
            case = manifest['cases'][0]
            case.pop('native')
            case.update(status='failed', error='client timed out')
            path.write_text(json.dumps(manifest))
            row = evidence.validate(path, root)[0]
            self.assertEqual(row['status'], 'failed')
            self.assertNotIn('rps', row)


if __name__ == '__main__':
    unittest.main()
