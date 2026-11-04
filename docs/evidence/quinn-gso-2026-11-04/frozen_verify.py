#!/usr/bin/env python3
"""Recompute complete traffic records and compare GSO modes without hiding failures."""
import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import statistics

ROOT = Path(__file__).resolve().parents[3]


def verify(root, binaries):
    spec = importlib.util.spec_from_file_location('http_gso_verify', ROOT / 'scripts/perf/http-state/verify.py')
    http = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(http)
    suite = json.loads((root / 'suite.json').read_text())
    for name, sha in suite['binary_sha256'].items():
        assert hashlib.sha256((binaries / name).read_bytes()).hexdigest() == sha
    assert len(suite['runs']) == 16
    expected = {(r, mode, size, state) for r in range(4) for mode in ['auto', 'off']
                for size, state in [(32768, 'hit'), (131072, 'off')]}
    assert {(r['round'], r['mode'], r['bytes'], r['cache']) for r in suite['runs']} == expected
    records = []
    for run in suite['runs']:
        path = root / run['manifest']
        manifest = json.loads(path.read_text())
        assert manifest['source_revision'] == suite['source']
        assert manifest['client_sha256'] == suite['client_sha256']
        for filename, sha in manifest['binaries'].items():
            assert suite['binary_sha256'][Path(filename).name] == sha
        args = manifest['args']
        assert args['edge_backend'] == 'quinn-native' and args['transport_profile'] == 'matched'
        assert args['quinn_native_gso'] == run['mode'] and args['address_validation'] == 'off'
        assert args['fresh_requests'] == suite['fresh_requests_per_case'] == 16384
        assert len(run['native_listener_startup']) >= 2
        assert all('gso=' + ('Auto' if run['mode'] == 'auto' else 'Off') in line for line in run['native_listener_startup'])
        rows = http.validate(path, root)
        assert len(rows) == 2 and {r['connection'] for r in rows} == {'fresh', 'reuse'}
        for row in rows:
            assert row['protocol'] == 'h3' and row['payload_bytes'] == run['bytes'] and row['cache'] == run['cache']
            row.update(mode=run['mode'], round=run['round'])
        records.extend(rows)
    groups = defaultdict(lambda: defaultdict(list))
    for row in records:
        groups[row['payload_bytes'], row['cache'], row['connection']][row['mode']].append(row)
    comparisons = []
    for (size, cache, connection), modes in sorted(groups.items()):
        result = {'bytes': size, 'cache': cache, 'connection': connection, 'modes': {}}
        for mode, rows in modes.items():
            data = {'statuses': dict(Counter(r['status'] for r in rows)),
                    'successful': sum(r.get('successful', 0) for r in rows),
                    'failed': sum(r.get('failed', 0) for r in rows)}
            if len(rows) == 4 and all(r['status'] == 'passed' for r in rows):
                data['independent_medians'] = {k: statistics.median(r[k] for r in rows)
                    for k in ['rps', 'p99_ms', 'cpu_usec_per_request', 'cgroup_peak_mib',
                              'client_cpu_fraction', 'origin_cpu_fraction', 'proxy_cpu_fraction']}
            result['modes'][mode] = data
        if all(r['status'] == 'passed' for rows in modes.values() for r in rows):
            auto = sorted(modes['auto'], key=lambda r: r['round'])
            off = sorted(modes['off'], key=lambda r: r['round'])
            result['paired_off_auto'] = {field: [100 * (b[field] / a[field] - 1) for a, b in zip(auto, off)]
                for field in ['rps', 'cpu_usec_per_request', 'p99_ms']}
        comparisons.append(result)
    summary = {'source': suite['source'], 'cases': len(records),
               'statuses': dict(Counter(r['status'] for r in records)),
               'successful': sum(r.get('successful', 0) for r in records),
               'failed': sum(r.get('failed', 0) for r in records),
               'fresh_successful': sum(r.get('successful', 0) for r in records if r['connection'] == 'fresh'),
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
