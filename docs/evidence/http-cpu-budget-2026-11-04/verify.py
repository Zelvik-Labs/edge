#!/usr/bin/env python3
"""Recompute user/system CPU budgets after the full frozen node verification."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
import statistics
import subprocess
import sys

HERE = Path(__file__).resolve().parent


def derive(root):
    suite = json.loads((root / 'suite.json').read_text())
    assert suite['source'] == '4171ea700386d8ed7c755255c7eeccb7e804d644'
    rows = []
    groups = defaultdict(list)
    for run in suite['runs']:
        manifest = json.loads((root / run['manifest']).read_text())
        assert manifest['status'] == 'completed' and not run['infrastructure_failure']
        assert len(manifest['cpu_assignment']['client']) == 2
        assert len(manifest['cpu_assignment']['proxy']) == 1
        for case in manifest['cases']:
            assert case['status'] == 'passed' and case['native']['failed'] == 0
            count = case['native']['successful']
            assert count > 0
            before, after = [case[f'proxy_{side}']['cpu.stat'] for side in ['before', 'after']]
            delta = {name: after[name] - before[name] for name in ['usage_usec', 'user_usec', 'system_usec']}
            assert all(value >= 0 for value in delta.values())
            seconds = case['elapsed']
            row = dict(round=run['round'], target=run['variant'], protocol=case['protocol'], cache=case['cache'],
                       successful=count, cpu_delta_usec=delta,
                       total_usec_per_request=delta['usage_usec'] / count,
                       user_usec_per_request=delta['user_usec'] / count,
                       system_usec_per_request=delta['system_usec'] / count,
                       rps=case['native']['good_rps'],
                       client_cpu_fraction=case['client_cpu_seconds'] / (2 * seconds),
                       proxy_cpu_fraction=delta['usage_usec'] / (seconds * 1e6))
            rows.append(row)
            groups[row['protocol'], row['cache'], row['target']].append(row)
    assert len(rows) == 192 and sum(row['successful'] for row in rows) == 6854656
    fields = ['total_usec_per_request', 'user_usec_per_request', 'system_usec_per_request',
              'rps', 'client_cpu_fraction', 'proxy_cpu_fraction']
    medians = []
    for (protocol, cache, target), selected in sorted(groups.items()):
        assert len(selected) == 4 and {row['round'] for row in selected} == set(range(4))
        medians.append(dict(protocol=protocol, cache=cache, target=target,
                            **{field: statistics.median(row[field] for row in selected) for field in fields}))
    return dict(source=suite['source'], runtime=suite['build']['variants']['after']['source'],
                cases=len(rows), successful=sum(row['successful'] for row in rows),
                independent_medians=medians, records=rows)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('evidence', type=Path)
    parser.add_argument('binaries', type=Path)
    args = parser.parse_args()
    subprocess.run([sys.executable, str(HERE.parent / 'stored-headers-2026-11-04/node/verify.py'),
                    str(args.evidence), str(args.binaries)], check=True)
    actual = derive(args.evidence / 'extracted')
    assert actual == json.loads((HERE / 'cpu-budget.json').read_text())
    print(json.dumps({key: actual[key] for key in ['source', 'runtime', 'cases', 'successful']}))
