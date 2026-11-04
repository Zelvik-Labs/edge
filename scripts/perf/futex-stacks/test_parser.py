#!/usr/bin/env python3
"""Guard caller attribution and argument exclusion in the public representation."""
import importlib.util
import json
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location('futex_helper', Path(__file__).with_name('helper.py'))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)

class ParserTests(unittest.TestCase):
    def test_unfinished_wait_and_wake_keep_their_callers_without_arguments(self):
        text = '''futex(0xdeadbeef, FUTEX_WAIT_BITSET_PRIVATE|FUTEX_CLOCK_REALTIME, 7, NULL, FUTEX_BITSET_MATCH_ANY <unfinished ...>
<... futex resumed>) = 0
 > /lib/libc.so.6(futex_wait+0x27) [0xcafef00d]
 > /test/Edge(worker+0x42) [0xabcdef01]
futex(0xdeadbeef, FUTEX_WAKE_PRIVATE, 1) = 1
 > /test/Edge(notify+0x19) [0xabcdef02]
+++ exited with 0 +++
'''
        rows = helper.parse(text)
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]['operation'], 'FUTEX_WAIT_BITSET_PRIVATE|FUTEX_CLOCK_REALTIME')
        self.assertEqual(len(rows[0]['frames']), 2)
        self.assertIn('notify', rows[1]['frames'][0])
        public = json.dumps(rows)
        for secret in ['0x', 'deadbeef', 'cafef00d', 'NULL', 'MATCH_ANY']:
            self.assertNotIn(secret, public)
        summary = helper.summarize(rows)
        self.assertEqual(sum(summary['operations'].values()), summary['events'])
        self.assertEqual(sum(row['calls'] for row in summary['caller_chains']), 2)

    def test_unsupported_lines_fail_instead_of_silently_losing_evidence(self):
        with self.assertRaises(ValueError):
            helper.parse('unexpected future strace format')
        with self.assertRaises(AssertionError):
            helper.parse(' > /test/Edge(orphan+0x19) [0x1234]')

    def test_empty_thread_and_pending_entry_are_explicit(self):
        self.assertEqual(helper.parse(''), [])
        summary = helper.summarize(helper.parse('futex(0x1234, FUTEX_WAIT_PRIVATE, 2, NULL <unfinished ...>'))
        self.assertEqual(summary['events'], 1)
        self.assertEqual(summary['stackless_events'], 1)

if __name__ == '__main__':
    unittest.main()
