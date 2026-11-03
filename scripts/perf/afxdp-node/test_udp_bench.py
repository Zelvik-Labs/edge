#!/usr/bin/env python3
"""Behavior checks for the real byte/sequence-validating load generator."""
import json
import os
from pathlib import Path
import signal
import statistics
import subprocess
import tempfile
import time
import unittest


class EchoBenchmarkTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = Path(os.environ['UDP_BENCH_BINARY']).resolve(strict=True)
        cls.output = Path(os.environ['UDP_BENCH_TEST_OUTPUT']).resolve()
        cls.output.mkdir(parents=True, exist_ok=True)

    def exercise(self, concurrency, payload, corrupt=False, gated=False):
        output = Path(tempfile.mkdtemp(prefix=self._testMethodName + '-', dir=self.output))
        ready, origin_result = output/'ready.json', output/'origin.json'
        client_result, samples_path = output/'client.json', output/'samples.json'
        with (output/'origin.log').open('w') as log:
            origin = subprocess.Popen([str(self.binary), 'origin', '127.0.0.1', '0', str(ready),
                                       str(origin_result), 'corrupt' if corrupt else 'valid'],
                                      stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 5
                while not ready.exists():
                    self.assertIsNone(origin.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.01)
                port = json.loads(ready.read_text())['port']
                with (output/'client.log').open('w') as client_log:
                    command = [str(self.binary), 'gated-client' if gated else 'client', '127.0.0.1', str(port),
                                             str(concurrency), '32', '4', str(payload),
                                             str(client_result), str(samples_path)]
                    if gated:
                        warm, go, done = output/'warm.json', output/'go', output/'done.json'
                        command += [str(warm), str(go), str(done)]
                        child = subprocess.Popen(command, stdout=client_log, stderr=subprocess.STDOUT)
                        deadline = time.monotonic() + 5
                        while not warm.exists():
                            self.assertIsNone(child.poll())
                            self.assertLess(time.monotonic(), deadline)
                            time.sleep(.001)
                        self.assertFalse(client_result.exists())
                        go.write_text('go\n')
                        result = subprocess.CompletedProcess(command, child.wait(timeout=15))
                        self.assertTrue(done.exists())
                    else:
                        result = subprocess.run(command, stdout=client_log, stderr=subprocess.STDOUT, timeout=15)
            finally:
                origin.send_signal(signal.SIGTERM)
                self.assertEqual(origin.wait(timeout=5), 0)
        measured = json.loads(client_result.read_text())
        echoed = json.loads(origin_result.read_text())
        self.assertEqual(result.returncode, 1 if corrupt else 0)
        if corrupt:
            self.assertFalse(measured['passed'])
            self.assertIn('mismatch', measured['error'])
            self.assertNotIn('requests_per_second', measured)
            self.assertFalse(samples_path.exists())
            return
        self.assertTrue(measured['passed'])
        self.assertEqual(measured['sent'], concurrency * 36)
        self.assertEqual(measured['received'], concurrency * 36)
        self.assertEqual(echoed['received'], concurrency * 36)
        self.assertEqual(echoed['sent'], concurrency * 36)
        self.assertEqual(echoed['distinct_peer_ports'], concurrency)
        samples = json.loads(samples_path.read_text())
        self.assertEqual(len(samples), concurrency * 32)
        self.assertTrue(all(value > 0 for value in samples))
        ordered = sorted(samples)
        self.assertEqual(measured['p99_rtt_ns'], ordered[(len(samples) * 99 + 99)//100 - 1])
        self.assertEqual(measured['p50_rtt_ns'], ordered[(len(samples) - 1)//2])
        self.assertAlmostEqual(measured['mean_rtt_ns'], statistics.fmean(samples), delta=.001)
        self.assertAlmostEqual(measured['requests_per_second'],
                               len(samples)*1e9/measured['elapsed_ns'], delta=.0001)
        self.assertEqual(measured['timed_end_ns']-measured['timed_start_ns'], measured['elapsed_ns'])
        self.assertGreaterEqual(measured['elapsed_ns'], max(samples))
        self.assertGreaterEqual(measured['timed_cpu_user_seconds'], 0)
        self.assertGreaterEqual(measured['timed_cpu_system_seconds'], 0)

    def test_single_flow_small_payload(self):
        self.exercise(1, 64)

    def test_concurrent_small_payload(self):
        self.exercise(16, 64)

    def test_concurrent_large_payload(self):
        self.exercise(16, 1200)

    def test_warmup_gate_releases_the_measured_phase(self):
        self.exercise(16, 1200, gated=True)

    def test_corruption_is_a_failure_and_never_a_throughput_result(self):
        self.exercise(1, 64, True)


if __name__ == '__main__':
    unittest.main()
