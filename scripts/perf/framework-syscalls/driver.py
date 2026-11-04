#!/usr/bin/env python3
"""Observe all server threads with a separate, counts-only ptrace sidecar."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('syscall_matrix', ROOT / 'scripts/perf/protocol_state_matrix.py')
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)


def publish(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', choices=['edge', 'pingora', 'nginx', 'openresty'], required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tls', type=Path, required=True)
    options = parser.parse_args()
    original_docker, original_client = matrix.docker, matrix.run_client
    target = None

    def docker(*args):
        nonlocal target
        if args[0] == 'run' and '--name' in args:
            name = args[args.index('--name') + 1]
            if name.endswith('-' + options.target):
                target = name
        return original_docker(*args)

    def run_client(args, protocol, state, connection, url, ca, stem):
        if stem.name == 'origin-reference':
            return original_client(args, protocol, state, connection, url, ca, stem)
        assert target
        if options.target == 'edge':
            pids = [str(json.loads((stem.parent / 'edge-ready.json').read_text())['edge_pid'])]
        elif options.target == 'pingora':
            pids = ['1']
        else:
            paths = original_docker('exec', target, 'sh', '-c',
                'for p in /proc/[0-9]*/comm; do read n < "$p"; '
                'case "$n" in nginx|openresty) echo "$p";; esac; done').splitlines()
            pids = [str(pid) for pid in sorted({int(Path(p).parent.name) for p in paths})]
        assert pids
        identities = {pid: original_docker('exec', target, 'sha256sum', f'/proc/{pid}/exe').split()[0] for pid in pids}
        assert len(set(identities.values())) == 1
        matrix.write(Path(str(stem) + '.trace-target.json'), dict(target=options.target,
            container=target, container_pids=list(map(int, pids)), executable_sha256=next(iter(identities.values()))))
        if options.target in ['nginx', 'openresty']:
            executable = stem.parent / 'server-executable'
            if not executable.exists():
                remote = original_docker('exec', target, 'readlink', f'/proc/{pids[0]}/exe')
                assert remote.startswith('/') and not remote.endswith(' (deleted)')
                subprocess.run([*matrix.DOCKER, 'cp', target + ':' + remote, str(executable)], env=matrix.ENV, check=True)
            assert matrix.sha(executable) == next(iter(identities.values()))
        trace = Path(str(stem) + '.trace')
        trace.mkdir()
        remote = '/test/' + trace.name
        sidecar = 'edge-syscall-sidecar-' + uuid.uuid4().hex[:10]
        command = ['run', '-d', '--name', sidecar, '--network', 'none', '--pid', 'container:' + target,
            '--cpuset-cpus', str(args.cpu_assignment['client'][0]), '--cpus', '1',
            '--memory', '256m', '--memory-swap', '256m', '--cap-add', 'SYS_PTRACE',
            '--security-opt', 'seccomp=unconfined',
            '--mount', f'type=bind,src={stem.parent},dst=/test',
            '--mount', f'type=bind,src={ROOT}/scripts/perf/framework-syscalls/helper.py,dst=/tools/helper.py,readonly',
            '--mount', f'type=bind,src={ROOT}/scripts/perf/afxdp-node/syscall_profile.py,dst=/tools/syscall_profile.py,readonly',
            '--entrypoint', 'python3', 'edge-syscall-sidecar:20261104', '/tools/helper.py',
            '--directory', remote, '--max-seconds', '60', '--pids', *pids]
        matrix.write(trace / 'sidecar-command.json', [*matrix.DOCKER, *command])
        ended = False
        try:
            original_docker(*command)
            deadline = time.monotonic() + 35
            while not (trace / 'ready.json').exists():
                assert time.monotonic() < deadline, 'sidecar attachment timed out'
                status = json.loads(original_docker('inspect', sidecar))[0]['State']['Status']
                if status != 'running':
                    raise RuntimeError('sidecar failed attachment: ' + original_docker('logs', sidecar)[-2000:])
                time.sleep(.02)
            started = time.monotonic_ns()
            result = original_client(args, protocol, state, connection, url, ca, stem)
            publish(trace / 'load-ended.json', dict(completed=True, started_ns=started, ended_ns=time.monotonic_ns()))
            ended = True
            code = original_docker('wait', sidecar)
            matrix.write(trace / 'sidecar-exit.json', {'exit': int(code)})
            assert code == '0', original_docker('logs', sidecar)[-2000:]
            return result
        finally:
            if not ended and (trace / 'ready.json').exists():
                publish(trace / 'load-ended.json', dict(completed=False, ended_ns=time.monotonic_ns()))
                try:
                    stopped = subprocess.run([*matrix.DOCKER, 'wait', sidecar], env=matrix.ENV,
                        capture_output=True, text=True, timeout=15)
                    matrix.write(trace / 'sidecar-exit.json', dict(cli_exit=stopped.returncode,
                        container_exit=stopped.stdout.strip(), completed=False))
                except subprocess.TimeoutExpired:
                    matrix.write(trace / 'trace-failure.json', dict(error='failed tracer did not detach within 15s', completed=False))
            names = original_docker('ps', '-a', '--format', '{{.Names}}').splitlines()
            if sidecar in names:
                original_docker('rm', '-f', sidecar)

    matrix.docker, matrix.run_client = docker, run_client
    bins = ROOT / '.local/architecture-profile-binaries'
    args = argparse.Namespace(target=options.target, output=options.output, tls=options.tls,
        client=bins / '.local/http-state-ci/protocol-state-client',
        edge_binary=bins / 'target/release/Zelvik-Edge', controller=bins / 'target/release/examples/issue34-controller',
        bare_pingora_binary=ROOT / '.local/framework-profile-binaries/bare-pingora-proxy',
        seconds=10, repeats=1, payload_size=1024, concurrency=32, cold_requests=None, fresh_requests=2048,
        client_source_revision='a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a',
        protocols=['h1', 'h2'], states=['off'], connections=['reuse'], multiplex=False, seed=20261104,
        edge_image='edge-http-state-fixture:20261103', edge_backend='quinn-shared' if options.target == 'edge' else options.target,
        experiment='counts-only ptrace with checked thread coverage and separate tracer cgroup; exclude timing from capacity ranking')
    return matrix.run(args)


if __name__ == '__main__':
    raise SystemExit(main())
