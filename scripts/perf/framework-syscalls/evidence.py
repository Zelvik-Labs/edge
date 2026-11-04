#!/usr/bin/env python3
"""Check actual syscall coverage and every transfer; never rank ptrace timing."""
import argparse
from collections import defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/framework-syscalls-private'
PUBLIC = ROOT / '.local/framework-syscalls-ci'
ORDERS = [['edge', 'pingora', 'nginx', 'openresty'], ['openresty', 'nginx', 'pingora', 'edge']]


def module(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run():
    original = module('syscall_framework_inputs', 'scripts/perf/framework-profile/evidence.py')
    original.PRIVATE = PRIVATE
    original.prepare()
    source = PRIVATE / 'source'
    source.mkdir()
    client_revision = 'a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a'
    (source / 'protocol_state_client.c').write_bytes(subprocess.check_output(
        ['git', 'show', client_revision + ':benches/support/protocol_state_client.c']))
    suite = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'client_source_revision': client_revision,
        'client_source_sha256': sha(source / 'protocol_state_client.c'),
        'orders': ORDERS, 'scope': 'ptrace syscall counts only; tracer shares one client CPU but has a separate cgroup and no network; exclude all timing from capacity rankings',
        'source_hashes': {p: sha(ROOT / p) for p in ['scripts/perf/framework-syscalls/driver.py',
            'scripts/perf/framework-syscalls/helper.py', 'scripts/perf/afxdp-node/syscall_profile.py']},
        'images': {}, 'runs': []}
    for image in ['nginx:1.28.0', 'openresty/openresty:1.27.1.2-0-bookworm-fat',
                  'edge-http-state-fixture:20261103', 'edge-syscall-sidecar:20261104']:
        data = json.loads(subprocess.check_output(['docker', 'image', 'inspect', image], text=True))[0]
        suite['images'][image] = {'id': data['Id'], 'repo_digests': data['RepoDigests']}
    (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
    for number, order in enumerate(ORDERS):
        for target in order:
            output = PRIVATE / f'round-{number}-{target}'
            with (PRIVATE / f'{number}-{target}.driver.log').open('w') as log:
                result = subprocess.run(['python3', 'scripts/perf/framework-syscalls/driver.py',
                    '--target', target, '--output', str(output), '--tls', str(PRIVATE / 'tls')],
                    stdout=log, stderr=subprocess.STDOUT)
            suite['runs'].append(dict(round=number, target=target,
                manifest=str(output.relative_to(PRIVATE) / 'manifest.json'), exit=result.returncode))
            (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
            if result.returncode:
                raise RuntimeError(f'syscall diagnostic failed: round {number} {target}; partial files retained')
    verify()


def verify():
    http = module('syscall_wire', 'scripts/perf/http-state/verify.py')
    parser = module('syscall_parser', 'scripts/perf/afxdp-node/syscall_profile.py')
    suite = json.loads((PRIVATE / 'suite.json').read_text())
    provenance = json.loads((PRIVATE / 'provenance.json').read_text())
    bare = json.loads((PRIVATE / 'bare-build-metadata.json').read_text())
    assert sha(PRIVATE / 'source/protocol_state_client.c') == suite['client_source_sha256']
    assert [(r['round'], r['target']) for r in suite['runs']] == [(i, t) for i, order in enumerate(ORDERS) for t in order]
    rows = []
    for run_data in suite['runs']:
        assert run_data['exit'] == 0
        path = PRIVATE / run_data['manifest']
        manifest = json.loads(path.read_text())
        assert manifest['source_revision'] == suite['source']
        assert manifest['client_sha256'] == provenance['expected_binaries']['.local/http-state-ci/protocol-state-client']
        assert manifest['client_source_revision'] == suite['client_source_revision']
        assert manifest['client_source_sha256'] == suite['client_source_sha256']
        checked = http.validate(path, PRIVATE)
        assert len(checked) == 2 and all(r['status'] == 'passed' and r['failed'] == 0 for r in checked)
        assert {(r['protocol'], r['cache']) for r in checked} == {('h1', 'off'), ('h2', 'off')}
        cases = {r['stem']: r for r in manifest['cases']}
        for row in checked:
            stem = path.parent / row['stem']
            trace = Path(str(stem) + '.trace')
            assert json.loads((trace / 'sidecar-exit.json').read_text())['exit'] == 0
            report = json.loads((trace / 'syscall-summary.json').read_text())
            parsed = parser.parse_summary((trace / 'syscall-summary.log').read_text())
            assert parsed['syscalls'] == report['syscalls'] and parsed['total'] == report['total']
            assert report['completed'] and report['attached_before']['threads'] and report['attached_after']['threads']
            assert report['detached_threads']
            ready = json.loads((trace / 'ready.json').read_text())
            load = json.loads((trace / 'load-ended.json').read_text())
            assert load['completed'] and ready['tracer_pid'] == report['tracer_pid']
            assert ready['monotonic_ns'] == report['attached_before']['monotonic_ns']
            assert ready['monotonic_ns'] <= load['started_ns'] <= load['ended_ns'] <= report['attached_after']['monotonic_ns'] <= report['stop_requested_monotonic_ns'] <= report['stopped_monotonic_ns']
            command = json.loads((trace / 'sidecar-command.json').read_text())
            assert command[command.index('--network') + 1] == 'none'
            assert command[command.index('--cpuset-cpus') + 1] == str(manifest['cpu_assignment']['client'][0])
            identity = json.loads(Path(str(stem) + '.trace-target.json').read_text())
            assert identity['container_pids'] == report['pids']
            expected = provenance['expected_binaries']['target/release/Zelvik-Edge'] if run_data['target'] == 'edge' else bare['bare_binary']['sha256'] if run_data['target'] == 'pingora' else sha(trace / 'server-executable')
            assert set(ready['executable_sha256_by_pid']) == {str(pid) for pid in report['pids']}
            assert set(ready['executable_sha256_by_pid'].values()) == {expected}
            assert identity['target'] == run_data['target'] and identity['executable_sha256'] == expected
            assert identity['container'] == command[command.index('--pid') + 1].removeprefix('container:')
            assert cases[row['stem']]['status'] == 'passed'
            rows.append(dict(round=run_data['round'], target=run_data['target'], protocol=row['protocol'],
                successful=row['successful'], total_syscalls=report['total']['calls'],
                syscall_errors=report['total']['errors'],
                calls_per_response=report['total']['calls'] / row['successful'],
                named_calls_per_response={name: data['calls'] / row['successful'] for name, data in report['syscalls'].items()}))
    groups = defaultdict(list)
    for row in rows:
        groups[row['target'], row['protocol']].append(row)
    medians = []
    for (target, protocol), data in sorted(groups.items()):
        assert len(data) == 2
        names = set().union(*(r['named_calls_per_response'] for r in data))
        medians.append(dict(target=target, protocol=protocol,
            total=statistics.median(r['calls_per_response'] for r in data),
            named={name: statistics.median(r['named_calls_per_response'].get(name, 0) for r in data) for name in sorted(names)}))
    summary = {'source': suite['source'], 'scope': suite['scope'], 'cells': len(rows),
        'successful': sum(r['successful'] for r in rows), 'failed': 0,
        'syscall_errors_are_not_http_errors': True, 'strace_wall_time_is_not_cpu_time': True,
        'records': rows, 'medians': medians}
    (PRIVATE / 'verified-syscalls.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k not in ['records', 'medians']}))


def collect():
    PUBLIC.mkdir(parents=True, exist_ok=True)
    exact = {'suite.json', 'provenance.json', 'bare-build-metadata.json', 'verified-syscalls.json',
        'manifest.json', 'payload', 'server-executable', 'ready.json', 'load-ended.json', 'sidecar-exit.json',
        'sidecar-command.json', 'syscall-summary.json', 'syscall-summary.log', 'tracer.log',
        'syscall-aborted.json', 'trace-failure.json', 'protocol_state_client.c'}
    suffixes = ('.client.tsv.gz', '.client.tsv', '.stderr', '.command.json', '.trace-target.json', '.driver.log', '.body', '.headers')
    retained = []
    if PRIVATE.exists():
        for path in PRIVATE.rglob('*'):
            if path.is_file() and (path.name in exact or path.name.endswith(suffixes)):
                relative = path.relative_to(PRIVATE)
                output = PUBLIC / relative
                output.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, output)
                retained.append(dict(path=str(relative), bytes=output.stat().st_size, sha256=sha(output)))
    (PUBLIC / 'retained-manifest.json').write_text(json.dumps({'files': retained,
        'excluded': ['TLS private keys', 'runtime snapshots and stores', 'raw syscall arguments and payload memory']}, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['run', 'verify', 'collect'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        PRIVATE = args.directory.resolve()
    globals()[args.mode]()
