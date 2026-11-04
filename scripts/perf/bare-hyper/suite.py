#!/usr/bin/env python3
"""Run five independent OFF-only targets and verify every transfer."""
import argparse
from collections import defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/bare-hyper-private'
OUT = ROOT / '.local/bare-hyper-ci'
TARGETS = ['edge', 'pingora', 'hyper', 'nginx', 'openresty']
ORDERS = [TARGETS[offset:] + TARGETS[:offset] for offset in range(5)]


def run():
    suite = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
             'orders': ORDERS, 'runs': []}
    (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
    failed = False
    for number, order in enumerate(ORDERS):
        for target in order:
            output = PRIVATE / f'round-{number}-{target}'
            with (PRIVATE / f'{number}-{target}.driver.log').open('w') as log:
                result = subprocess.run(['python3', 'scripts/perf/bare-hyper/single.py',
                    '--target', target, '--output', str(output), '--tls', str(PRIVATE / 'tls'),
                    '--seed', str(20261104 + number)], stdout=log, stderr=subprocess.STDOUT)
            suite['runs'].append(dict(round=number, target=target, manifest=str(output.relative_to(PRIVATE) / 'manifest.json'), exit=result.returncode))
            (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
            failed |= result.returncode != 0
    verify()
    if failed:
        raise RuntimeError('one or more independent framework cells failed; no successful retry replaces them')


def verify():
    suite = json.loads((PRIVATE / 'suite.json').read_text())
    build = json.loads((OUT / 'build-metadata.json').read_text())
    assert build['clippy_exit'] == build['build_exit'] == 0
    for relative, expected in build['sources'].items():
        assert hashlib.sha256((OUT / 'source' / relative).read_bytes()).hexdigest() == expected
    assert hashlib.sha256((OUT / 'bare-pingora-proxy').read_bytes()).hexdigest() == build['pingora_binary']['sha256']
    binary = OUT / 'bare-hyper-proxy'
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == build['bare_binary']['sha256']
    assert build['all_compilation_complete_before_measurement']
    assert [(r['round'], r['target']) for r in suite['runs']] == [(i, target) for i, order in enumerate(ORDERS) for target in order]
    spec = importlib.util.spec_from_file_location('bare_wire_verify', ROOT / 'scripts/perf/http-state/verify.py')
    http = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(http)
    records = []
    for run_data in suite['runs']:
        path = PRIVATE / run_data['manifest']
        manifest = json.loads(path.read_text())
        rows = http.validate(path, PRIVATE)
        assert run_data['exit'] == 0 and len(rows) == 3 and all(r['status'] == 'passed' for r in rows)
        assert manifest['source_revision'] == suite['source']
        assert manifest['client_sha256'] == build['expected_inputs']['protocol-state-client']
        assert manifest['client_source_revision'] == 'a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a'
        assert manifest['client_source_sha256'] == hashlib.sha256(subprocess.check_output(['git', 'show', manifest['client_source_revision'] + ':benches/support/protocol_state_client.c'], cwd=ROOT)).hexdigest()
        if run_data['target'] in ['edge', 'pingora', 'hyper']:
            expected = [build['expected_inputs']['Zelvik-Edge'], build['expected_inputs']['issue34-controller']] if run_data['target'] == 'edge' else [build['pingora_binary']['sha256']] if run_data['target'] == 'pingora' else [build['bare_binary']['sha256']]
            assert sorted(manifest['binaries'].values()) == sorted(expected)
        identities = json.loads((path.parent / 'server-identities.json').read_text())
        expected_server = build['expected_inputs']['Zelvik-Edge'] if run_data['target'] == 'edge' else build['pingora_binary']['sha256'] if run_data['target'] == 'pingora' else build['bare_binary']['sha256'] if run_data['target'] == 'hyper' else hashlib.sha256((path.parent / 'server-executable').read_bytes()).hexdigest()
        assert identities and all(row['executable_sha256'] == expected_server for row in identities)
        assert {row['protocol'] for row in identities} == {'h1', 'h1-tls', 'h2'}
        controls = json.loads((path.parent / 'tls-controls.json').read_text())
        assert len(controls) == 2 and {r['protocol'] for r in controls} == {'h1-tls', 'h2'} and all(r['exit'] == 60 for r in controls)
        observed = json.loads((path.parent / 'origin-connections.json').read_text())
        snapshots = {r['requests']: r for r in observed}
        assert len(snapshots) == len(observed)
        cases = {c['stem']: c for c in manifest['cases']}
        for row in rows:
            case = cases[row['stem']]
            before = snapshots[case['origin_counter_before']]
            after = snapshots[case['origin_counter_after']]
            connections = after['accepted'] - before['accepted'] - 1
            assert connections >= 0 and row['successful'] >= 1000
            assert connections < row['successful'] / 4, 'upstream reuse control failed'
            row.update(round=run_data['round'], target=run_data['target'], origin_new_connections=connections)
            before_cpu, after_cpu = [case['proxy_process_cpu_' + side] for side in ['before', 'after']]
            assert before_cpu['clock_ticks_per_second'] == after_cpu['clock_ticks_per_second'] > 0
            before_processes = {r['pid']: r for r in before_cpu['processes']}
            assert set(before_processes) == {r['pid'] for r in after_cpu['processes']}
            roles = dict.fromkeys(['edge_process', 'controller', 'other'] if run_data['target'] == 'edge' else ['proxy'], 0.0)
            observed_roles = set()
            for data in after_cpu['processes']:
                old = before_processes[data['pid']]
                assert old['start_ticks'] == data['start_ticks'] and old['comm'] == data['comm']
                ticks = data['user_ticks'] + data['system_ticks'] - old['user_ticks'] - old['system_ticks']
                assert ticks >= 0
                role = 'proxy'
                if run_data['target'] == 'edge':
                    role = 'edge_process' if 'Zelvik-Edge' in data['comm'] else 'controller' if 'controller' in data['comm'] else 'other'
                observed_roles.add(role)
                roles[role] += ticks * 1e6 / before_cpu['clock_ticks_per_second'] / row['successful']
            if run_data['target'] == 'edge':
                assert {'edge_process', 'controller'} <= observed_roles
            row['process_cpu_usec_per_request'] = dict(roles)
            records.append(row)
    groups = defaultdict(list)
    for row in records:
        groups[row['protocol'], row['target']].append(row)
    summary = {'source': suite['source'], 'edge_runtime': build['edge_runtime'],
               'scope': build['scope'], 'cells': len(records), 'successful': sum(r['successful'] for r in records),
               'failed': sum(r['failed'] for r in records), 'wrong_ca_controls': 50,
               'max_host_steal_fraction': max(r['max_host_steal_fraction'] for r in records), 'medians': []}
    for (protocol, target), rows in sorted(groups.items()):
        assert len(rows) == 5
        summary['medians'].append(dict(protocol=protocol, target=target,
            **{field: statistics.median(r[field] for r in rows) for field in
               ['rps', 'cpu_usec_per_request', 'p99_ms', 'client_cpu_fraction', 'proxy_cpu_fraction', 'origin_cpu_fraction', 'origin_new_connections']}))
        summary['medians'][-1]['process_cpu_usec_per_request'] = {
            role: statistics.median(r['process_cpu_usec_per_request'][role] for r in rows)
            for role in rows[0]['process_cpu_usec_per_request']}
    (OUT / 'verified-records.json').write_text(json.dumps(records, indent=2) + '\n')
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['run', 'verify'])
    args = parser.parse_args()
    globals()[args.mode]()
