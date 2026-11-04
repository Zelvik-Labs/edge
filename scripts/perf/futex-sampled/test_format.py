#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest
ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('sampled_driver', ROOT / 'scripts/perf/profile_architecture.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class FormatTests(unittest.TestCase):
    def test_tracepoint_payload_is_reduced_to_operation(self):
        raw = 'Edge 31/32 1.2: 64 syscalls:sys_enter_futex: uaddr: 0xdeadbeef, op: 0x00000080, val: 0x1234\n\t7fff wake+0x1 (/test/Edge)\n\n'
        public = module.sanitize_futex_script(raw)
        self.assertIn('syscalls:sys_enter_futex: operation=128', public)
        self.assertIn('wake+0x1', public)
        for field in ['uaddr', 'val:', 'deadbeef', '0x1234']:
            self.assertNotIn(field, public)

    def test_unknown_operation_format_fails_before_publication(self):
        with self.assertRaises(AssertionError):
            module.sanitize_futex_script('Edge 1: syscalls:sys_enter_futex: unknown payload')

    def test_effective_kernel_attributes_are_required(self):
        module.verify_effective_period('syscalls:sys_enter_futex: { sample_period, sample_freq }: 512, disabled: 1\nsyscalls:sys_enter_futex: sample_period=512', 512)
        for report in ['sample_period=1', 'sample_freq=512', 'sample_period=512\nsample_freq=512', '{ sample_period, sample_freq }: 512', 'missing attrs']:
            with self.assertRaises(AssertionError):
                module.verify_effective_period(report, 512)

if __name__ == '__main__':
    unittest.main()
