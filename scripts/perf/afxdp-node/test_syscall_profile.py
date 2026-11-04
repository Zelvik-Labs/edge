"""Real ptrace attach/detach against an explicitly consenting owned process."""
import json
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from syscall_profile import SyscallProfile, parse_summary


CHILD = r'''
import ctypes, os, sys, threading
from pathlib import Path
libc = ctypes.CDLL(None, use_errno=True)
scope = Path('/proc/sys/kernel/yama/ptrace_scope')
if scope.exists() and int(scope.read_text()) != 0:
    result = libc.prctl(0x59616d61, ctypes.c_ulong(-1), 0, 0, 0)
    if result != 0:
        raise OSError(ctypes.get_errno(), 'owned fixture ptracer opt-in failed')
stop = threading.Event()
worker = threading.Thread(target=stop.wait)
worker.start()
print('ready', flush=True)
for command in sys.stdin:
    if command.strip() == 'quit':
        break
    assert command.strip() == 'run'
    for _ in range(200):
        os.getpid()
    print('done', flush=True)
stop.set()
worker.join()
'''


class ProfileTests(unittest.TestCase):
    def test_attach_covers_threads_counts_and_detaches(self):
        with tempfile.TemporaryDirectory(prefix='edge-owned-ptrace-') as name:
            child = subprocess.Popen([sys.executable, '-c', CHILD], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, text=True)
            tracer = None
            try:
                self.assertEqual(child.stdout.readline().strip(), 'ready')
                tracer = SyscallProfile([child.pid], Path(name))
                self.assertGreaterEqual(len(tracer.attach()['threads']), 2)
                child.stdin.write('run\n');child.stdin.flush()
                self.assertEqual(child.stdout.readline().strip(), 'done')
                tracer.check_after_measurement()
                report = tracer.finish()
                self.assertEqual(report['syscalls']['getpid']['calls'], 200)
                self.assertEqual(report, json.loads((Path(name)/'syscall-summary.json').read_text()))
                for task in Path(f'/proc/{child.pid}/task').iterdir():
                    state = dict(line.split(':', 1) for line in (task/'status').read_text().splitlines() if ':' in line)
                    self.assertEqual(int(state['TracerPid'].strip()), 0)
            finally:
                if tracer is not None and not tracer.stopped:
                    tracer.abort()
                if child.poll() is None:
                    child.stdin.write('quit\n');child.stdin.flush()
                self.assertEqual(child.wait(timeout=10), 0)
                child.stdin.close();child.stdout.close()

    def test_early_tracer_exit_is_an_error(self):
        with tempfile.TemporaryDirectory(prefix='edge-owned-ptrace-') as name:
            child = subprocess.Popen([sys.executable, '-c', CHILD], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, text=True)
            tracer = None
            try:
                self.assertEqual(child.stdout.readline().strip(), 'ready')
                tracer = SyscallProfile([child.pid], Path(name))
                tracer.attach()
                tracer.process.send_signal(signal.SIGKILL)
                tracer.process.wait(timeout=10)
                with self.assertRaisesRegex(RuntimeError, 'before requested detach'):
                    tracer.finish()
            finally:
                if tracer is not None and not tracer.stopped:
                    tracer.abort()
                if child.poll() is None:
                    child.stdin.write('quit\n');child.stdin.flush()
                self.assertEqual(child.wait(timeout=10), 0)
                child.stdin.close();child.stdout.close()

    def test_inconsistent_counts_are_rejected(self):
        with self.assertRaises(AssertionError):
            parse_summary('100.00 0.001 1 10 getpid\n100.00 0.001 1 11 total\n')

    def test_abort_detaches_without_completed_profile(self):
        with tempfile.TemporaryDirectory(prefix='edge-owned-ptrace-') as name:
            child = subprocess.Popen([sys.executable, '-c', CHILD], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, text=True)
            tracer = None
            try:
                self.assertEqual(child.stdout.readline().strip(), 'ready')
                tracer = SyscallProfile([child.pid], Path(name))
                tracer.attach()
                tracer.abort()
                self.assertTrue(tracer.stopped)
                self.assertFalse(json.loads((Path(name)/'syscall-aborted.json').read_text())['completed'])
                self.assertFalse((Path(name)/'syscall-summary.json').exists())
            finally:
                if tracer is not None and not tracer.stopped:
                    tracer.abort()
                if child.poll() is None:
                    child.stdin.write('quit\n');child.stdin.flush()
                self.assertEqual(child.wait(timeout=10), 0)
                child.stdin.close();child.stdout.close()


if __name__ == '__main__':
    unittest.main()
