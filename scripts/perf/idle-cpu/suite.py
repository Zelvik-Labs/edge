#!/usr/bin/env python3
"""Run each proxy independently; preserve counter windows and every failure."""
import argparse
from collections import defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/idle-cpu-private'
OUT = ROOT / '.local/idle-cpu-ci'
ORDERS = [['edge', 'pingora', 'nginx', 'openresty'], ['openresty', 'nginx', 'pingora', 'edge']]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def write(path, data):
    path.write_text(json.dumps(data, indent=2) + '\n')


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run():
    suite = dict(source=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                 orders=ORDERS, runs=[])
    write(PRIVATE / 'suite.json', suite)
    failed = False
    for number, order in enumerate(ORDERS):
        for target in order:
            output = PRIVATE / f'round-{number}-{target}'
            with (PRIVATE / f'{number}-{target}.driver.log').open('w') as log:
                result = subprocess.run(['python3', 'scripts/perf/idle-cpu/single.py',
                    '--target', target, '--output', str(output), '--tls', str(PRIVATE / 'tls'),
                    '--seed', str(20261104 + number)], stdout=log, stderr=subprocess.STDOUT)
            suite['runs'].append(dict(round=number, target=target,
                manifest=str(output.relative_to(PRIVATE) / 'manifest.json'), exit=result.returncode))
            write(PRIVATE / 'suite.json', suite)
            failed |= result.returncode != 0
    verify()
    if failed:
        raise RuntimeError('one or more diagnostic fixtures failed; original failures retained')


def verify():
    observer = module('recheck_idle_observer', ROOT / 'scripts/perf/idle-cpu/observer.py')
    http = module('idle_wire_verify', ROOT / 'scripts/perf/http-state/verify.py')
    suite = json.loads((PRIVATE / 'suite.json').read_text())
    build = json.loads((OUT / 'build-metadata.json').read_text())
    assert suite['source'] == build['source'] and build['no_compilation']
    assert [(r['round'], r['target']) for r in suite['runs']] == [(i, target) for i, order in enumerate(ORDERS) for target in order]
    for name, digest in build['sources'].items():
        assert sha(OUT / 'source' / name) == digest
    assert sha(OUT / 'bare-pingora-proxy') == build['expected_inputs']['bare-pingora-proxy']
    records, assignments = [], []
    for run_data in suite['runs']:
        path = PRIVATE / run_data['manifest']
        manifest = json.loads(path.read_text())
        assert run_data['exit'] == 0 and manifest['source_revision'] == suite['source']
        assert manifest['client_sha256'] == build['expected_inputs']['protocol-state-client']
        revision = 'a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a'
        assert manifest['client_source_revision'] == revision
        assert manifest['client_source_sha256'] == hashlib.sha256(subprocess.check_output(
            ['git', 'show', revision + ':benches/support/protocol_state_client.c'], cwd=ROOT)).hexdigest()
        checked = http.validate(path, PRIVATE)
        assert len(checked) == 1 and checked[0]['status'] == 'passed' and checked[0]['failed'] == 0
        assert (checked[0]['protocol'], checked[0]['cache'], checked[0]['connection']) == ('h1', 'off', 'reuse')
        assignment = manifest['cpu_assignment']
        flat = [cpu for group in assignment.values() for cpu in group]
        assert len(flat) == len(set(flat))
        assignments.append(assignment)
        expected = (build['expected_inputs']['Zelvik-Edge'] if run_data['target'] == 'edge' else
                    build['expected_inputs']['bare-pingora-proxy'] if run_data['target'] == 'pingora' else
                    sha(path.parent / 'server-executable'))
        identity = json.loads((path.parent / 'server-identities.json').read_text())
        assert identity and all(row['executable_sha256'] == expected and row['protocol'] == 'h1' for row in identity)
        assert {row['stem'] for row in identity} == {'readiness', manifest['cases'][0]['stem'] + '-before', manifest['cases'][0]['stem'] + '-after'}
        if run_data['target'] == 'edge':
            assert sorted(manifest['binaries'].values()) == sorted(build['expected_inputs'][key] for key in ['Zelvik-Edge', 'issue34-controller'])
        elif run_data['target'] == 'pingora':
            assert list(manifest['binaries'].values()) == [expected]
        context = json.loads((path.parent / 'cpu-context.json').read_text())
        case = manifest['cases'][0]
        origin = json.loads((path.parent / 'origin-connections.json').read_text())
        counters = {row['requests']: row for row in origin}
        assert len(counters) == len(origin)
        connections = counters[case['origin_counter_after']]['accepted'] - counters[case['origin_counter_before']]['accepted'] - 1
        assert 0 <= connections < checked[0]['successful'] / 4
        stem = path.parent / case['stem']
        phases = {}
        snapshots = []
        for phase in ['before', 'after']:
            data = json.loads(Path(str(stem) + '-' + phase + '.idle.json').read_text())
            assert data['complete'] and data['requested_seconds'] == 10 and len(data['snapshots']) == 11
            recalculated = observer.delta(data['snapshots'][0], data['snapshots'][-1], context['host_ticks_per_second'])
            assert json.loads(json.dumps(recalculated)) == data['summary']
            assert recalculated['seconds'] >= 10
            phases[phase] = recalculated
            snapshots.extend(data['snapshots'])
        loaded = json.loads(Path(str(stem) + '.load-cpu.json').read_text())
        assert loaded['complete']
        recalculated = observer.delta(loaded['before'], loaded['after'], context['host_ticks_per_second'])
        assert json.loads(json.dumps(recalculated)) == loaded['summary']
        snapshots.extend([loaded['before'], loaded['after']])
        pids = {row['process']['pid']: row['process']['start_ticks'] for row in snapshots[0]['processes']}
        for sample in snapshots:
            assert {row['process']['pid']: row['process']['start_ticks'] for row in sample['processes']} == pids
            quota, period = map(int, sample['cpu_max'].split())
            assert quota == period * len(assignment['proxy']) and period > 0
            assert sample['memory_events']['oom'] == sample['memory_events']['oom_kill'] == 0
        assert set(context['pids']) == set(pids)
        record = dict(round=run_data['round'], target=run_data['target'],
            successful=checked[0]['successful'], failed=checked[0]['failed'],
            idle_before=phases['before'], loaded=recalculated, idle_after=phases['after'],
            loaded_cpu_usec_per_response=recalculated['cpu_usec']['usage_usec'] / checked[0]['successful'],
            loaded_max_host_steal_fraction=checked[0]['max_host_steal_fraction'],
            idle_memory_peak_bytes=max(row['memory_peak'] for row in snapshots),
            cpu_assignment=assignment, executable_sha256=expected,
            no_idle_cpu_subtraction=True, no_capacity_ranking=True)
        records.append(record)
    assert assignments and all(row == assignments[0] for row in assignments)
    groups = defaultdict(list)
    for row in records:
        groups[row['target']].append(row)
    medians = [dict(target=target, idle_before_cpu_cores=statistics.median(row['idle_before']['cpu_cores'] for row in rows),
        loaded_cpu_usec_per_response=statistics.median(row['loaded_cpu_usec_per_response'] for row in rows),
        idle_after_cpu_cores=statistics.median(row['idle_after']['cpu_cores'] for row in rows))
        for target, rows in sorted(groups.items())]
    summary = dict(source=suite['source'], cells=len(records), successful=sum(row['successful'] for row in records),
        failed=sum(row['failed'] for row in records), cpu_assignment=assignments[0], medians=medians,
        no_idle_cpu_subtraction=True, no_capacity_ranking=True,
        scope='host-counter diagnosis of immutable executables, H1 OFF reuse, two independent reverse orders')
    write(OUT / 'verified-records.json', records)
    write(OUT / 'verified-summary.json', summary)
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['run', 'verify'])
    globals()[parser.parse_args().mode]()
