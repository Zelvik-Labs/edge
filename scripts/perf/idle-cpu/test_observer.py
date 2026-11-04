#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('observer', Path(__file__).with_name('observer.py'))
observer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(observer)


class CounterBoundaries(unittest.TestCase):
    def test_proc_stat_comm_can_contain_spaces_and_parentheses(self):
        fields = ['S'] + ['0'] * 21
        fields[11], fields[12], fields[19] = '15', '7', '900'
        value = observer.stat('123 (worker (idle) x) ' + ' '.join(fields))
        self.assertEqual(value, dict(pid=123, comm='worker (idle) x', state='S',
                                    user_ticks=15, system_ticks=7, start_ticks=900))

    def test_process_identity_reuse_is_rejected(self):
        def sample(start):
            return dict(monotonic_ns=start, cpu=dict(usage_usec=10, user_usec=8, system_usec=2),
                        processes=[dict(process=dict(pid=1, start_ticks=start, user_ticks=1, system_ticks=1), threads=[])])
        with self.assertRaises(AssertionError):
            observer.delta(sample(1), sample(2), 100)

    def test_thread_exit_does_not_hide_whole_process_cpu(self):
        old = dict(pid=1, start_ticks=10, comm='edge', user_ticks=10, system_ticks=2)
        new = dict(old, user_ticks=30, system_ticks=5)
        thread = dict(pid=2, start_ticks=11, comm='short', user_ticks=7, system_ticks=1)
        a = dict(monotonic_ns=1, cpu=dict(usage_usec=0, user_usec=0, system_usec=0),
                 processes=[dict(process=old, threads=[thread])])
        b = dict(monotonic_ns=1000000001, cpu=dict(usage_usec=230000, user_usec=200000, system_usec=30000),
                 processes=[dict(process=new, threads=[])])
        result = observer.delta(a, b, 100)
        self.assertEqual(result['process_cpu'][0]['user_ticks'], 20)
        self.assertEqual(result['process_cpu'][0]['exited_thread_identities'], [(2, 11)])
        self.assertEqual(result['process_cpu'][0]['stable_threads'], [])
        self.assertEqual(result['cpu_cores'], .23)


if __name__ == '__main__':
    unittest.main()
