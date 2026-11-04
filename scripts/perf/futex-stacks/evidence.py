#!/usr/bin/env python3
"""Verify all sanitized futex records and independent HTTP controls."""
import argparse
from collections import Counter
import importlib.util
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/futex-stacks-private'
PUBLIC = ROOT / '.local/futex-stacks-ci'
ORDERS = [['edge', 'pingora'], ['pingora', 'edge']]
SCOPE = ('Futex operation and userspace caller attribution under ptrace; '
         'all timing excluded from capacity comparison, no wait duration or per-component CPU attribution')


def module(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def run():
    original = module('futex_original_counts', 'scripts/perf/framework-syscalls/evidence.py')
    original.PRIVATE, original.PUBLIC = PRIVATE, PUBLIC
    original.ORDERS, original.SCOPE = ORDERS, SCOPE
    original.DRIVER = 'scripts/perf/futex-stacks/driver.py'
    original.EXTRA_IMAGES = ['edge-futex-sidecar:20261104']
    original.OBSERVER_SOURCES = ['scripts/perf/futex-stacks/' + name
        for name in ['helper.py', 'driver.py', 'evidence.py', 'Dockerfile', 'test_parser.py']]
    original.OBSERVER_SOURCES += ['scripts/perf/framework-syscalls/driver.py',
        'scripts/perf/framework-syscalls/evidence.py', 'scripts/perf/afxdp-node/syscall_profile.py']
    original.verify = verify
    original.run()


def verify():
    http = module('futex_http', 'scripts/perf/http-state/verify.py')
    helper = module('futex_sanitized', 'scripts/perf/futex-stacks/helper.py')
    suite = json.loads((PRIVATE / 'suite.json').read_text())
    provenance = json.loads((PRIVATE / 'provenance.json').read_text())
    bare = json.loads((PRIVATE / 'bare-build-metadata.json').read_text())
    assert suite['scope'] == SCOPE
    assert [(r['round'], r['target']) for r in suite['runs']] == [(i, target) for i, order in enumerate(ORDERS) for target in order]
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
        assert len(checked) == 2 and {(r['protocol'], r['cache']) for r in checked} == {('h1', 'off'), ('h2', 'off')}
        assert all(r['status'] == 'passed' and r['failed'] == 0 for r in checked)
        for row in checked:
            stem = path.parent / row['stem']
            trace = Path(str(stem) + '.trace')
            assert json.loads((trace / 'sidecar-exit.json').read_text())['exit'] == 0
            report = json.loads((trace / 'futex-summary.json').read_text())
            events = json.loads((trace / 'futex-events.json').read_text())
            rebuilt = helper.summarize(events)
            assert all(report[key] == value for key, value in rebuilt.items())
            assert report['completed'] and not report['raw_arguments_published']
            assert all(set(event) == {'operation', 'frames', 'tid'} for event in events)
            assert all('0x' not in frame for event in events for frame in event['frames'])
            ready = json.loads((trace / 'ready.json').read_text())
            load = json.loads((trace / 'load-ended.json').read_text())
            assert load['completed'] and ready['tracer_pid'] == report['tracer_pid']
            assert ready['monotonic_ns'] == report['attached_before']['monotonic_ns']
            assert ready['monotonic_ns'] <= load['started_ns'] <= load['ended_ns'] <= report['attached_after']['monotonic_ns'] <= report['stop_requested_monotonic_ns'] <= report['stopped_monotonic_ns']
            tids = {r['tid'] for r in report['threads']}
            assert set(report['attached_before']['threads']) <= tids and set(report['attached_after']['threads']) <= tids
            counts = Counter(event['tid'] for event in events)
            assert all(thread['events'] == counts[thread['tid']] for thread in report['threads'])
            assert report['detached_threads']
            command = json.loads((trace / 'sidecar-command.json').read_text())
            assert command[command.index('--network') + 1] == 'none'
            identity = json.loads(Path(str(stem) + '.trace-target.json').read_text())
            assert identity['container_pids'] == report['pids']
            expected = provenance['expected_binaries']['target/release/Zelvik-Edge'] if run_data['target'] == 'edge' else bare['bare_binary']['sha256']
            assert identity['target'] == run_data['target'] and identity['executable_sha256'] == expected
            assert set(ready['executable_sha256_by_pid']) == {str(pid) for pid in report['pids']}
            assert set(ready['executable_sha256_by_pid'].values()) == {expected}
            assert identity['container'] == command[command.index('--pid') + 1].removeprefix('container:')
            if run_data['target'] == 'edge':
                assert report['events'] > report['stackless_events'], 'no actual Edge caller attribution'
            rows.append(dict(round=run_data['round'], target=run_data['target'], protocol=row['protocol'],
                successful=row['successful'], events=report['events'], stackless=report['stackless_events'],
                operations=report['operations'], calls_per_response=report['events'] / row['successful'],
                caller_chains=report['caller_chains']))
    assert len(rows) == 8
    summary = dict(source=suite['source'], scope=SCOPE, cells=len(rows),
        successful=sum(row['successful'] for row in rows), failed=0, records=rows,
        raw_arguments_published=False, raw_syscall_logs_archived=False,
        limitation='Sanitized event records are retained and recomputed; private argument-bearing raw logs are excluded')
    (PRIVATE / 'verified-futex.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({key: value for key, value in summary.items() if key != 'records'}))


def collect():
    original = module('futex_counts_collector', 'scripts/perf/framework-syscalls/evidence.py')
    original.PRIVATE, original.PUBLIC = PRIVATE, PUBLIC
    original.collect()
    path = PUBLIC / 'retained-manifest.json'
    manifest = json.loads(path.read_text())
    for source in PRIVATE.rglob('*'):
        if source.is_file() and source.name in ['futex-events.json', 'futex-summary.json', 'detached.json', 'verified-futex.json']:
            relative = source.relative_to(PRIVATE)
            output = PUBLIC / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, output)
            manifest['files'].append(dict(path=str(relative), bytes=output.stat().st_size, sha256=original.sha(output)))
    path.write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['run', 'verify', 'collect'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        PRIVATE = args.directory.resolve()
    globals()[args.mode]()
