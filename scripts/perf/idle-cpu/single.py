#!/usr/bin/env python3
"""Use the independent full wire fixture and observe origin connection reuse."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('bare_matrix', ROOT / 'scripts/perf/protocol_state_matrix.py')
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--target', choices=['edge', 'pingora', 'nginx', 'openresty'], required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--tls', type=Path, required=True)
parser.add_argument('--seed', type=int, required=True)
options = parser.parse_args()
observations = []
tls_controls = []


def origin_count(port):
    accepted, handled, requests = map(int, matrix.raw_curl(f'http://127.0.0.1:{port}/_stats').decode().splitlines()[2].split())
    observations.append(dict(port=port, accepted=accepted, handled=handled, requests=requests, observed_at=time.time()))
    matrix.write(options.output / 'origin-connections.json', observations)
    return requests


original_probe = matrix.probe
original_docker = matrix.docker
current_target = None
identities = []


def docker(*command):
    global current_target
    if command[0] == 'run' and '--name' in command:
        name = command[command.index('--name') + 1]
        if name.endswith('-' + options.target):
            current_target = name
    return original_docker(*command)


matrix.docker = docker


observer_spec = importlib.util.spec_from_file_location('idle_observer', ROOT / 'scripts/perf/idle-cpu/observer.py')
observer = importlib.util.module_from_spec(observer_spec)
observer_spec.loader.exec_module(observer)
host_context = None


def context():
    global host_context
    if host_context is None:
        init_pid = int(original_docker('inspect', '--format', '{{.State.Pid}}', current_target))
        top = original_docker('top', current_target, '-eo', 'pid,comm')
        pids = [int(row.split()[0]) for row in top.splitlines() if row.split() and row.split()[0].isdigit()]
        assert init_pid in pids and pids
        folder = observer.cgroup_path(init_pid)
        host_context = dict(folder=folder, pids=pids, init_pid=init_pid)
        matrix.write(options.output / 'cpu-context.json',
            dict(cgroup_path=str(folder), pids=pids, init_pid=init_pid,
                 host_ticks_per_second=os.sysconf('SC_CLK_TCK'),
                 scope='host namespace and read-only host cgroup counters; no in-target observer'))
    return host_context


original_client = matrix.run_client


def run_client(args, protocol, state, connection, url, ca, stem):
    selected = context()
    before = observer.snapshot(selected['folder'], selected['pids'], matrix.host_cpu)
    complete = False
    try:
        raw = original_client(args, protocol, state, connection, url, ca, stem)
        complete = True
        return raw
    finally:
        after = observer.snapshot(selected['folder'], selected['pids'], matrix.host_cpu)
        matrix.write(Path(str(stem) + '.load-cpu.json'), dict(complete=complete,
            before=before, after=after,
            summary=observer.delta(before, after, os.sysconf('SC_CLK_TCK'))))


matrix.run_client = run_client


def probe(protocol, url, ca, payload, stem):
    result = original_probe(protocol, url, ca, payload, stem)
    if current_target:
        pid = str(json.loads((stem.parent / 'edge-ready.json').read_text())['edge_pid']) if options.target == 'edge' else '1'
        actual = original_docker('exec', current_target, 'sha256sum', '/proc/' + pid + '/exe').split()[0]
        if options.target in ['nginx', 'openresty']:
            executable = options.output / 'server-executable'
            if not executable.exists():
                remote = original_docker('exec', current_target, 'readlink', '/proc/' + pid + '/exe')
                assert remote.startswith('/') and not remote.endswith(' (deleted)')
                subprocess.run([*matrix.DOCKER, 'cp', current_target + ':' + remote, str(executable)], env=matrix.ENV, check=True)
            assert matrix.sha(executable) == actual
        else:
            expected = ROOT / '.local/idle-cpu-binaries/Zelvik-Edge' if options.target == 'edge' else ROOT / ('.local/idle-cpu-ci/bare-' + options.target + '-proxy')
            assert matrix.sha(expected) == actual
        identities.append(dict(stem=stem.name, protocol=protocol, pid=int(pid), executable_sha256=actual))
        matrix.write(options.output / 'server-identities.json', identities)
    if protocol in ['h1-tls', 'h2'] and protocol not in {r['protocol'] for r in tls_controls}:
        command = ['curl', '--noproxy', '*', '--silent', '--show-error', '--max-time', '20',
                   matrix.FLAGS[protocol], '--cacert', str(options.tls / 'wrong-ca.pem'), url]
        negative = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        tls_controls.append({'protocol': protocol, 'exit': negative.returncode, 'command': command,
                             'stdout_bytes': len(negative.stdout), 'stderr': negative.stderr.decode()})
        matrix.write(options.output / 'tls-controls.json', tls_controls)
        assert negative.returncode == 60, tls_controls[-1]
    if current_target and stem.name.endswith(('-before', '-after')):
        idle = observer.interval(context()['folder'], context()['pids'], matrix.host_cpu,
                                 matrix.write, Path(str(stem) + '.idle.json'))
        idle.update(scope='no benchmark clients; before/after native load; background work remains enabled')
        matrix.write(Path(str(stem) + '.idle.json'), idle)
    return result


matrix.origin_count = origin_count
matrix.probe = probe
bins = ROOT / '.local/idle-cpu-binaries'
args = argparse.Namespace(target=options.target, output=options.output, tls=options.tls,
    client=bins / 'protocol-state-client', edge_binary=bins / 'Zelvik-Edge', controller=bins / 'issue34-controller',
    bare_pingora_binary=ROOT / '.local/idle-cpu-ci/bare-pingora-proxy',
    client_source_revision='a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a',
    edge_image='edge-http-state-fixture:20261103', seconds=5, repeats=1, payload_size=1024, concurrency=32,
    edge_backend='quinn-shared' if options.target == 'edge' else options.target,
    capture_proxy_process_cpu=True,
    parent_cgroup_root=observer.cgroup_path(os.getpid()),
    cold_requests=None, fresh_requests=2048, protocols=['h1'], states=['off'],
    connections=['reuse'], multiplex=False, seed=options.seed,
    experiment='idle and loaded CPU diagnosis of fixed ELFs; observers use host counters; no subtraction or capacity ranking')
raise SystemExit(matrix.run(args))
