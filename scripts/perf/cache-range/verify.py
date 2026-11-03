#!/usr/bin/env python3
"""Recompute every transfer and paired comparison; retain incomplete cells."""
import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import statistics

ROOT = Path(__file__).resolve().parents[3]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(root, binaries):
    spec = importlib.util.spec_from_file_location('http_range_verify', ROOT / 'scripts/perf/http-state/verify.py')
    http = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(http)
    suite = json.loads((root / 'suite.json').read_text())
    build = suite['build']
    assert build == json.loads((root / 'build-metadata.json').read_text())
    assert build == json.loads((binaries / 'build-metadata.json').read_text())
    assert build['builds_complete_before_measurement']
    for variant, data in build['variants'].items():
        for name, details in data['binaries'].items():
            path = binaries / variant / name
            assert path.stat().st_size == details['bytes'] and digest(path) == details['sha256']
    assert len(suite['runs']) == 16
    assert [(r['round'], r['variant']) for r in suite['runs']] == [
        (i, target) for i, order in enumerate(suite['orders']) for target in order]
    records = []
    for run in suite['runs']:
        path = root / run['manifest']
        manifest = json.loads(path.read_text())
        assert manifest['source_revision'] == suite['source']
        assert manifest['client_sha256'] == suite['client_sha256']
        variant = run['variant']
        if variant in ['before', 'after']:
            assert manifest['args']['binary_source'] == build['variants'][variant]['source']
            for filename, sha in manifest['binaries'].items():
                assert build['variants'][variant]['binaries'][Path(filename).name]['sha256'] == sha
        rows = http.validate(path, root)
        assert len(rows) == 12
        assert {(r['protocol'], r['cache'], r['connection']) for r in rows} == {
            (p, c, 'reuse') for p in ['h1', 'h1-tls', 'h2', 'h3'] for c in ['hit', 'off', 'miss']}
        for row in rows:
            row.update(round=run['round'], variant=variant)
        records.extend(rows)
    controls = json.loads((root / 'range-controls.json').read_text())
    control_failures = []
    for row in controls:
        index = {'bytes=0-15': 0, 'bytes=-16': 1}[row['range']]
        body = root / (row['case'] + f'.range-{index}.body')
        actual = digest(body) if body.exists() else None
        assert actual == row['body_sha256']
        if row['exit'] or row['output'] != f"{http.VERSIONS[row['protocol']]}\t0\t206" or actual != row['expected_sha256']:
            control_failures.append(row)
    statuses = Counter(r['status'] for r in records)
    if statuses == {'passed': 192}:
        assert len(controls) == 128 and not control_failures
    groups = defaultdict(lambda: defaultdict(list))
    for row in records:
        groups[row['protocol'], row['cache']][row['variant']].append(row)
    comparisons = []
    for (protocol, cache), targets in sorted(groups.items()):
        cell = {'protocol': protocol, 'cache': cache, 'targets': {}, 'paired_after_before': None}
        for variant, rows in sorted(targets.items()):
            data = {'statuses': dict(Counter(r['status'] for r in rows)), 'rounds': len(rows)}
            if len(rows) == 4 and all(r['status'] == 'passed' for r in rows):
                data['independent_medians'] = {field: statistics.median(r[field] for r in rows)
                    for field in ['rps', 'p99_ms', 'cpu_usec_per_request', 'client_cpu_fraction',
                                  'origin_cpu_fraction', 'proxy_cpu_fraction', 'cgroup_peak_mib']}
                data['max_host_steal_fraction'] = max(r['max_host_steal_fraction'] for r in rows)
            cell['targets'][variant] = data
        before = sorted(targets['before'], key=lambda r: r['round'])
        after = sorted(targets['after'], key=lambda r: r['round'])
        if len(before) == len(after) == 4 and all(r['status'] == 'passed' for r in before + after):
            ratios = {field: [(b[field] / a[field] - 1) * 100 for a, b in zip(before, after)]
                      for field in ['rps', 'cpu_usec_per_request', 'p99_ms']}
            cell['paired_after_before'] = {field: {'percent_changes': values,
                'median_percent': statistics.median(values), 'min_percent': min(values), 'max_percent': max(values)}
                for field, values in ratios.items()}
        comparisons.append(cell)
    summary = {'source': suite['source'], 'binary_sources': {k: v['source'] for k, v in build['variants'].items()},
               'cases': len(records), 'statuses': dict(statuses),
               'successful': sum(r.get('successful', 0) for r in records),
               'failed_transfers': sum(r.get('failed', 0) for r in records),
               'range_controls': len(controls), 'range_control_failures': len(control_failures),
               'max_host_steal_fraction': max(r.get('max_host_steal_fraction', 0) for r in records),
               'comparisons': comparisons}
    (root / 'verified-records.json').write_text(json.dumps(records, indent=2) + '\n')
    (root / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k != 'comparisons'}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('binaries', type=Path)
    args = parser.parse_args()
    verify(args.root, args.binaries)
