#!/usr/bin/env python3
"""Count syscalls under load, independently, using the unchanged production fixture.

ptrace changes timing substantially. Never use these runs as capacity results.
The tracer has its own cgroup and no network; only the target PID namespace is shared.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import time
import uuid


spec = importlib.util.spec_from_file_location(
    'matrix', Path(__file__).with_name('protocol_state_matrix.py'))
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)


def main(options):
    original_docker, original_client = matrix.docker, matrix.run_client
    target = None

    def docker(*args):
        nonlocal target
        if args[0] == 'run' and ('/tools/controller' in args or '/test/proxy.conf' in args):
            target = args[args.index('--name') + 1]
        return original_docker(*args)

    def run_client(args, protocol, state, connection, url, ca, stem):
        if stem.name == 'origin-reference':
            return original_client(args, protocol, state, connection, url, ca, stem)
        sidecar = 'edge-arch-trace-' + uuid.uuid4().hex[:10]
        if args.target == 'edge':
            pids = [str(json.loads((stem.parent / 'edge-ready.json').read_text())['edge_pid'])]
        else:
            # Read comm only; no process environments or command-line secrets.
            paths = original_docker('exec', target, 'sh', '-c',
                'for p in /proc/[0-9]*/comm; do read n < "$p"; '
                'case "$n" in nginx|openresty) echo "$p";; esac; done').splitlines()
            pids = [str(int(Path(p).parent.name)) for p in paths]
        if not pids:
            raise RuntimeError('target server process not found')
        remote = '/test/' + stem.name
        original_docker('run', '-d', '--name', sidecar, '--network', 'none',
            '--pid', 'container:' + target, '--cpuset-cpus', '0,1', '--cpus', '1',
            '--memory', '256m', '--memory-swap', '256m', '--cap-add', 'SYS_PTRACE',
            '--security-opt', 'seccomp=unconfined', '--mount',
            f'type=bind,src={stem.parent},dst=/test', '--entrypoint', 'sleep',
            options.profiler_image, '120')
        recorder = None
        try:
            trace_args = ['strace', '-f', '-c', '-o', remote + '.strace.txt']
            for pid in pids:
                trace_args += ['-p', pid]
            command = [*matrix.DOCKER, 'exec', sidecar, 'sh', '-c',
                       'echo $$ > "$1"; shift; exec "$@"', 'trace-launch',
                       remote + '.tracer.pid', *trace_args]
            matrix.write(Path(str(stem) + '.strace-command.json'), command)
            with Path(str(stem) + '.strace.log').open('x') as log:
                recorder = subprocess.Popen(command, env=matrix.ENV,
                                            stdout=log, stderr=subprocess.STDOUT)
                deadline = time.monotonic() + 10
                while not Path(str(stem) + '.tracer.pid').exists():
                    if recorder.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('tracer did not start')
                    time.sleep(.05)
                time.sleep(.2)
                if recorder.poll() is not None:
                    raise RuntimeError('tracer exited before load')
                result = original_client(args, protocol, state, connection, url, ca, stem)
                pid = str(int(Path(str(stem) + '.tracer.pid').read_text()))
                original_docker('exec', sidecar, 'kill', '-INT', pid)
                code = recorder.wait(timeout=15)
                if code not in (0, 130):
                    raise RuntimeError(f'tracer failed: {code}')
            if 'total' not in Path(str(stem) + '.strace.txt').read_text():
                raise RuntimeError('syscall summary incomplete')
            return result
        finally:
            original_docker('rm', '-f', sidecar)
            if recorder is not None and recorder.poll() is None:
                recorder.wait(timeout=15)

    matrix.docker, matrix.run_client = docker, run_client
    args = argparse.Namespace(
        target=options.target, output=options.output, tls=options.tls, client=options.client,
        edge_binary=Path('/workspace/edge/target/release/Zelvik-Edge'),
        controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
        seconds=options.seconds, repeats=1, payload_size=1024, concurrency=32,
        cold_requests=options.cold_requests, fresh_requests=2048, protocols=['h1'],
        states=options.states, connections=['reuse'], multiplex=False, seed=20261102,
        experiment='ptrace syscall counts; exclude from capacity comparison',
        profiler_image=options.profiler_image, trace_driver_sha256=matrix.sha(__file__))
    return matrix.run(args)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', choices=['edge', 'nginx', 'openresty'], required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tls', type=Path, required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--profiler-image', default='edge-gap-profiler:20261101')
    parser.add_argument('--seconds', type=int, default=5)
    parser.add_argument('--cold-requests', type=int, default=1024)
    parser.add_argument('--states', nargs='+', choices=['off', 'hit', 'miss'], default=['off', 'hit', 'miss'])
    raise SystemExit(main(parser.parse_args()))
