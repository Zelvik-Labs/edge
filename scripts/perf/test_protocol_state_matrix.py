#!/usr/bin/env python3
"""Regression gates for cache-state and new-connection evidence."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('matrix', Path(__file__).with_name('protocol_state_matrix.py'))
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)


class EvidenceTests(unittest.TestCase):
    def test_cpu_roles_use_actual_ids_and_do_not_overlap(self):
        roles = matrix.cpu_assignment([3, 5, 8, 9])
        self.assertEqual(roles, {'proxy':[3], 'client':[5,8], 'origin':[9]})
        roles = matrix.cpu_assignment([3, 5, 8, 9, 12])
        self.assertEqual(roles, {'proxy':[3,5], 'client':[8,9], 'origin':[12]})
        with self.assertRaises(RuntimeError): matrix.cpu_assignment([1, 2, 7])

    def test_reused_or_wrong_protocol_transfers_fail_the_fresh_connection_gate(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder)/'curl.tsv'
            path.write_text('200\t2\t1\t1024\t0.002\t0.001\t0\n'
                            '200\t2\t0\t1024\t0.002\t0.001\t0\n'
                            '200\t1.1\t1\t1024\t0.002\t0.001\t0\n')
            rows, failures = matrix.checked_curl_rows(path, 'h2', 1024, 3)
            self.assertEqual(len(rows), 3)
            self.assertEqual(len(failures), 2)

    def test_truncation_tls_error_and_missing_records_cannot_pass(self):
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'curl.tsv'
            path.write_text('200\t3\t1\t12\t0.002\t0.001\t0\n'
                            '000\t0\t1\t0\t0.002\t0\t60\n')
            _, failures=matrix.checked_curl_rows(path, 'h3', 1024, 2)
            self.assertEqual(len(failures),2)
            with self.assertRaisesRegex(RuntimeError,'expected 3'):
                matrix.checked_curl_rows(path,'h3',1024,3)

    def test_cache_hit_requires_zero_origin_requests_even_when_client_has_errors(self):
        for errors in [False, True]:
            with self.assertRaisesRegex(RuntimeError,'reached origin'):
                matrix.cache_contract('hit',1,100,errors)
        matrix.cache_contract('hit',0,100,False)

    def test_cold_and_disabled_cache_must_account_for_every_clean_response(self):
        for state in ['off','miss']:
            matrix.cache_contract(state,100,100,False)
            for origin in [0,99,101]:
                with self.assertRaises(RuntimeError):
                    matrix.cache_contract(state,origin,100,False)


if __name__=='__main__': unittest.main()
