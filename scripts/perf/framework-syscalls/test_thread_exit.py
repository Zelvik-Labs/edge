#!/usr/bin/env python3
"""A dead task may race procfs; a live unobservable/replaced process may not."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
spec = importlib.util.spec_from_file_location('thread_coverage', Path(__file__).resolve().parents[1] / 'afxdp-node/syscall_profile.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class ThreadExitTests(unittest.TestCase):
    def setUp(self):
        self.profile = object.__new__(module.SyscallProfile)
        self.profile.pid_start_times = {1: 111}
        self.profile.raced_thread_exits = []
        self.task = Path('/proc/1/task/51')

    def test_vanished_thread_is_recorded_under_same_live_process(self):
        with patch.object(Path, 'read_text', side_effect=FileNotFoundError), patch.object(Path, 'exists', return_value=False), patch.object(module.SyscallProfile, 'pid_start_time', return_value=111):
            self.assertIsNone(self.profile.task_status(1, self.task, 'after'))
        self.assertEqual(self.profile.raced_thread_exits, [dict(pid=1, tid=51, stage='after')])

    def test_live_thread_status_loss_is_rejected(self):
        with patch.object(Path, 'read_text', side_effect=FileNotFoundError), patch.object(Path, 'exists', return_value=True):
            with self.assertRaisesRegex(AssertionError, 'live thread'):
                self.profile.task_status(1, self.task, 'after')

    def test_process_exit_or_pid_reuse_is_rejected(self):
        for result in [FileNotFoundError(), 222]:
            with self.subTest(result=result), patch.object(Path, 'read_text', side_effect=FileNotFoundError), patch.object(Path, 'exists', return_value=False), patch.object(module.SyscallProfile, 'pid_start_time', side_effect=result if isinstance(result, Exception) else None, return_value=result):
                with self.assertRaises((FileNotFoundError, AssertionError)):
                    self.profile.task_status(1, self.task, 'after')
            self.assertEqual(self.profile.raced_thread_exits, [])

    def test_permission_failure_is_never_interpreted_as_exit(self):
        with patch.object(Path, 'read_text', side_effect=PermissionError):
            with self.assertRaises(PermissionError):
                self.profile.task_status(1, self.task, 'after')

if __name__ == '__main__':
    unittest.main()
