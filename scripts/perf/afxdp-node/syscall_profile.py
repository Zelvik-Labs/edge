"""Attach a counts-only tracer after warmup and verify its actual coverage."""
import json
from pathlib import Path
import signal
import subprocess
import time


def parse_summary(text):
    rows, total = {}, None
    for line in text.splitlines():
        fields = line.split()
        if not fields or fields[0] == '%' or set(fields[0]) == {'-'}:
            continue
        assert len(fields) in [5, 6], line
        row = {'wall_seconds': float(fields[1]), 'calls': int(fields[3]),
               'errors': int(fields[4]) if len(fields) == 6 else 0}
        assert row['calls'] >= row['errors'] >= 0 and row['wall_seconds'] >= 0
        if fields[-1] == 'total':
            assert total is None
            total = row
        else:
            assert fields[-1] not in rows
            rows[fields[-1]] = row
    assert rows and total and total['calls'] > 0
    assert total['calls'] == sum(row['calls'] for row in rows.values())
    assert total['errors'] == sum(row['errors'] for row in rows.values())
    return {'syscalls': rows, 'total': total}


class SyscallProfile:
    def __init__(self, pids, directory):
        self.pids = sorted(set(pids))
        assert self.pids
        self.directory = Path(directory)
        self.stopped = False
        self.ready = None
        self.after = None
        self.pid_start_times = {pid: self.pid_start_time(pid) for pid in self.pids}
        self.raced_thread_exits = []
        self.command = ['strace', '-f', '-c', '-w', '-qq', '-o', str(self.directory/'syscall-summary.log')]
        for pid in self.pids:
            self.command += ['-p', str(pid)]
        with (self.directory/'tracer.log').open('w') as log:
            self.process = subprocess.Popen(self.command, stdout=log, stderr=subprocess.STDOUT,
                                            start_new_session=True)

    @staticmethod
    def pid_start_time(pid):
        # comm may contain spaces or ')'; fields after its last ')' start
        # with state (field 3), so starttime (field 22) is index 19.
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        return int(fields[19])

    def task_status(self, pid, task, stage):
        try:
            return (task / 'status').read_text()
        except FileNotFoundError:
            # An enumerated thread can exit before procfs is read. Accept
            # only that vanished thread under the same still-live process;
            # a missing status on a live task, process exit/reuse, permission
            # failure or untraced live thread remains a hard failure.
            assert not task.exists(), 'live thread lost its status file'
            assert self.pid_start_time(pid) == self.pid_start_times[pid], 'target process replaced'
            self.raced_thread_exits.append(dict(pid=pid, tid=int(task.name), stage=stage))
            return None

    def attached_threads(self):
        assert self.process.poll() is None, f'tracer exited early: {self.process.returncode}'
        threads = []
        for pid in self.pids:
            assert self.pid_start_time(pid) == self.pid_start_times[pid], 'target process replaced'
            tasks = list(Path(f'/proc/{pid}/task').iterdir())
            assert tasks, pid
            for task in tasks:
                status = self.task_status(pid, task, 'before' if self.ready is None else 'after')
                if status is None:
                    continue
                fields = dict(line.split(':', 1) for line in status.splitlines() if ':' in line)
                if int(fields['TracerPid'].strip()) != self.process.pid:
                    return None
                threads.append(int(task.name))
        return sorted(threads)

    def attach(self):
        deadline = time.monotonic()+30
        while True:
            threads = self.attached_threads()
            if threads:
                self.ready = {'monotonic_ns': time.monotonic_ns(), 'threads': threads}
                return self.ready
            assert time.monotonic() < deadline, 'tracer attachment timed out'
            time.sleep(.002)

    def check_after_measurement(self):
        threads = self.attached_threads()
        assert threads, 'tracer did not retain all observed threads'
        self.after = {'monotonic_ns': time.monotonic_ns(), 'threads': threads}

    def _detach(self):
        assert not self.stopped, 'tracer already stopped'
        stop_requested = time.monotonic_ns()
        if self.process.poll() is not None:
            self.stopped = True
            raise RuntimeError(f'tracer exited before requested detach: {self.process.returncode}')
        self.process.send_signal(signal.SIGINT)
        try:
            code = self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            self.stopped = True
            raise RuntimeError('tracer failed graceful detach') from None
        self.stopped = True
        # With -p, strace may re-raise the SIGINT after flushing its summary.
        # This is accepted only after our own detach request; early exit above
        # is always an error, including an early zero or SIGINT status.
        assert code in [0, -signal.SIGINT], f'tracer detach exited {code}'
        detached = []
        for pid in self.pids:
            assert self.pid_start_time(pid) == self.pid_start_times[pid], 'target process replaced'
            for task in Path(f'/proc/{pid}/task').iterdir():
                status = self.task_status(pid, task, 'detach')
                if status is None:
                    continue
                fields = dict(line.split(':', 1) for line in status.splitlines() if ':' in line)
                assert int(fields['TracerPid'].strip()) == 0, f'thread {task.name} still traced'
                detached.append(int(task.name))
        return {'command': self.command, 'tracer_pid': self.process.pid, 'pids': self.pids,
                'pid_start_times': self.pid_start_times, 'raced_thread_exits': self.raced_thread_exits,
                'attached_before': self.ready, 'attached_after': self.after,
                'detached_threads': sorted(detached), 'requested_signal': signal.SIGINT,
                'stop_requested_monotonic_ns': stop_requested,
                'stopped_monotonic_ns': time.monotonic_ns(), 'exit_code': code}

    def finish(self):
        detached = self._detach()
        assert self.ready and self.after, 'trace did not cover a completed measurement'
        summary = parse_summary((self.directory/'syscall-summary.log').read_text())
        summary.update(detached)
        summary.update(completed=True,
                       scope='counts/wall time under ptrace, including snapshot boundary overhead; never uninstrumented capacity')
        (self.directory/'syscall-summary.json').write_text(json.dumps(summary, indent=2)+'\n')
        return summary

    def abort(self):
        report = self._detach()
        report.update(completed=False, scope='cleanup after incomplete measurement; partial raw log retained')
        (self.directory/'syscall-aborted.json').write_text(json.dumps(report, indent=2)+'\n')
