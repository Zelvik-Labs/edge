#!/usr/bin/env python3
"""Run the existing verified tracer in the target PID namespace, without network."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import time


def publish(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--pids', nargs='+', type=int, required=True)
    parser.add_argument('--max-seconds', type=int, required=True)
    parser.add_argument('--retain-executable', action='store_true')
    options = parser.parse_args()
    spec = importlib.util.spec_from_file_location('verified_syscalls', '/tools/syscall_profile.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    # Workers change UID and become non-dumpable. Read procfs identities in
    # this observer, which already has CAP_SYS_PTRACE, rather than changing
    # the measured server's capabilities or worker configuration.
    identities = {str(pid): hashlib.sha256(Path(f'/proc/{pid}/exe').read_bytes()).hexdigest()
                  for pid in options.pids}
    assert len(set(identities.values())) == 1
    if options.retain_executable:
        (options.directory / 'server-executable').write_bytes(Path(f'/proc/{options.pids[0]}/exe').read_bytes())
    profile = module.SyscallProfile(options.pids, options.directory)
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
        assert workload['completed'], 'incomplete client workload; retain partial syscall counts'
        assert identities == {str(pid): hashlib.sha256(Path(f'/proc/{pid}/exe').read_bytes()).hexdigest()
                              for pid in options.pids}, 'server executable changed during trace'
        profile.check_after_measurement()
        profile.finish()
    except BaseException as error:
        publish(options.directory / 'trace-failure.json', {'error': repr(error), 'completed': False})
        if not profile.stopped:
            profile.abort()
        raise


if __name__ == '__main__':
    main()
