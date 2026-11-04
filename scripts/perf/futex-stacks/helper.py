#!/usr/bin/env python3
"""Publish futex operation/caller records; keep raw syscall arguments private."""
import argparse
from collections import Counter
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import time


def publish(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def parse(text):
    events, current = [], None
    for line in text.splitlines():
        if line.startswith('futex('):
            match = re.match(r'futex\([^,]+,\s*([A-Z0-9_|]+),', line)
            assert match, 'unsupported futex operation syntax; raw log remains private'
            current = dict(operation=match[1], frames=[])
            events.append(current)
        elif re.match(r'^\s*>\s', line):
            assert current is not None, 'stack without a syscall entry'
            match = re.fullmatch(r'\s*>\s+(.*?)\s+\[0x[0-9a-f]+\]\s*', line)
            assert match, 'unsupported frame syntax; raw log remains private'
            frame = re.sub(r'0x[0-9a-f]+', '[offset]', match[1])
            current['frames'].append(frame)
        elif line.startswith('<... futex resumed>'):
            assert current is not None, 'resumed syscall without retained entry'
        elif not line or line.startswith(('--- ', '+++ ', 'strace:')):
            continue
        else:
            raise ValueError('unsupported syscall log line; raw log remains private')
    return events


def summarize(events):
    operations = Counter(row['operation'] for row in events)
    callers = Counter((row['operation'], tuple(row['frames'])) for row in events)
    return dict(events=len(events), operations=dict(operations),
        stackless_events=sum(not row['frames'] for row in events),
        caller_chains=[dict(operation=op, frames=list(frames), calls=count)
                       for (op, frames), count in callers.most_common()])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--pids', nargs='+', type=int, required=True)
    parser.add_argument('--max-seconds', type=int, required=True)
    options = parser.parse_args()
    spec = importlib.util.spec_from_file_location('verified_syscalls', '/tools/syscall_profile.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    class StackProfile(module.SyscallProfile):
        def __init__(self):
            self.pids = sorted(set(options.pids))
            self.directory = options.directory
            self.stopped, self.ready, self.after = False, None, None
            self.pid_start_times = {pid: self.pid_start_time(pid) for pid in self.pids}
            self.raced_thread_exits = []
            self.command = ['strace', '-ff', '-k', '--stack-trace-frame-limit=48', '-qq',
                '-e', 'trace=futex', '-o', str(self.directory / 'private-futex-raw')]
            for pid in self.pids:
                self.command += ['-p', str(pid)]
            with (self.directory / 'tracer.log').open('w') as log:
                self.process = subprocess.Popen(self.command, stdout=log, stderr=subprocess.STDOUT,
                                                start_new_session=True)

    identities = {str(pid): hashlib.sha256(Path(f'/proc/{pid}/exe').read_bytes()).hexdigest()
                  for pid in options.pids}
    assert len(set(identities.values())) == 1
    profile = StackProfile()
    try:
        ready = profile.attach()
        publish(options.directory / 'ready.json', dict(ready, tracer_pid=profile.process.pid,
            executable_sha256_by_pid=identities,
            strace_version=subprocess.check_output(['strace', '--version'], text=True).splitlines()[0]))
        deadline = time.monotonic() + options.max_seconds
        while not (options.directory / 'load-ended.json').exists():
            assert profile.process.poll() is None, 'tracer exited during workload'
            assert time.monotonic() < deadline, 'workload never published completion'
            time.sleep(.002)
        workload = json.loads((options.directory / 'load-ended.json').read_text())
        assert workload['completed'], 'incomplete client workload'
        assert identities == {str(pid): hashlib.sha256(Path(f'/proc/{pid}/exe').read_bytes()).hexdigest()
                              for pid in options.pids}, 'executable changed'
        profile.check_after_measurement()
        detached = profile._detach()
        publish(options.directory / 'detached.json', detached)
        paths = sorted(options.directory.glob('private-futex-raw.*'))
        assert paths, 'no per-thread tracer files'
        events, threads = [], []
        for path in paths:
            tid = int(path.suffix[1:])
            rows = parse(path.read_text())
            for row in rows:
                row['tid'] = tid
            events.extend(rows)
            threads.append(dict(tid=tid, events=len(rows)))
        frames = [frame for row in events for frame in row['frames']]
        if frames:
            demangled = subprocess.check_output(['c++filt', '-s', 'rust'],
                input='\n'.join(frames) + '\n', text=True).splitlines()
            assert len(demangled) == len(frames), 'demangler changed frame cardinality'
            offset = 0
            for row in events:
                count = len(row['frames'])
                row['frames'] = demangled[offset:offset + count]
                offset += count
        publish(options.directory / 'futex-events.json', events)
        publish(options.directory / 'futex-summary.json', dict(summarize(events),
            **detached, completed=True, threads=threads, raw_arguments_published=False,
            scope='futex entries and userspace caller chains under ptrace; no wait duration, CPU or capacity attribution'))
    except BaseException as error:
        publish(options.directory / 'trace-failure.json', dict(error=repr(error), completed=False))
        if not profile.stopped:
            profile.abort()
        raise


if __name__ == '__main__':
    main()
