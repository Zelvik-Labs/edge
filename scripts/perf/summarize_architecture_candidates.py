#!/usr/bin/env python3
"""Keep every baseline/candidate case and summarize only the recorded values."""
import argparse
from collections import defaultdict
import csv
import hashlib
import json
from pathlib import Path
import statistics


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def main(args):
    index = json.loads(args.index.read_text())
    if index['status'] != 'completed':
        raise ValueError('comparison is incomplete')
    records, runs = [], []
    groups = defaultdict(list)
    for entry in index['runs']:
        path = Path(entry['manifest'])
        if not path.is_absolute():
            path = args.index.parent / path
        manifest = json.loads(path.read_text())
        if manifest['status'] != 'completed' or manifest.get('cleanup_errors'):
            raise ValueError('incomplete fixture/cleanup')
        if any(not c.get('fresh_instance') for c in manifest['cases']):
            raise ValueError('mixed process/cache state')
        runs.append({**entry, 'manifest_sha256': sha(path),
                     'binaries': manifest.get('binaries'),
                     'driver_sha256': manifest['driver_sha256'],
                     'client_sha256': manifest['client_sha256'],
                     'images': manifest['images']})
        for case in manifest['cases']:
            native = case.get('native', {})
            row = {'label': entry['label'], 'round': entry['repeat'],
                   'payload_bytes': manifest['args']['payload_size'],
                   **{k: case[k] for k in ['protocol', 'cache', 'connection', 'status', 'stem']},
                   'error': case.get('error'), 'origin_requests': case.get('origin_requests'),
                   'elapsed': case.get('elapsed'),
                   'cache_probe_before_warm': case.get('integrity_before', {}).get('cache_status'),
                   'cache_probe_after_load': case.get('integrity_after', {}).get('cache_status'),
                   'cpu_usec_per_200': case.get('proxy_cpu_usec_per_200'),
                   'idle_cpu_cores': case.get('idle_reference', {}).get('cpu_cores'),
                   **{k: native.get(k) for k in ['good_rps', 'p99_ms', 'successful', 'failed', 'new_connections']}}
            row['peak_mib'] = case.get('proxy_after', {}).get('memory.peak', {}).get('bytes')
            if row['peak_mib'] is not None:
                row['peak_mib'] /= 2 ** 20
            steals = []
            for cpu in range(5):
                key = f'cpu{cpu}'
                if key in case.get('host_cpu_before', {}):
                    delta = [a-b for a,b in zip(case['host_cpu_after'][key], case['host_cpu_before'][key])]
                    steals.append(delta[7]/sum(delta[:8]) if sum(delta[:8]) else 0)
            row['max_core_steal'] = max(steals) if steals else None
            elapsed = row['elapsed']
            client_cpu = case.get('client_cpu_seconds')
            row['client_cpu_fraction_of_2cores'] = client_cpu / (2 * elapsed) if elapsed and client_cpu is not None else None
            row['origin_cpu_fraction'] = None
            if elapsed and case.get('origin_after'):
                row['origin_cpu_fraction'] = (case['origin_after']['cpu.stat']['usage_usec']
                    - case['origin_before']['cpu.stat']['usage_usec']) / (1e6 * elapsed)
            mount = '/node' if manifest['target'] == 'edge' else '/run/cache'
            row['data_filesystem_growth_mib'] = None
            if case.get('filesystem_after'):
                row['data_filesystem_growth_mib'] = (case['filesystem_after'][mount]['used_kib']
                    - case['filesystem_before'][mount]['used_kib']) / 1024
            records.append(row)
            groups[(row['payload_bytes'], row['protocol'], row['cache'], row['connection'], row['label'])].append(row)
    summaries = []
    for key, rows in sorted(groups.items()):
        values = {name + '_median': median(r[name] for r in rows) for name in [
            'good_rps', 'p99_ms', 'cpu_usec_per_200', 'peak_mib', 'new_connections', 'max_core_steal', 'idle_cpu_cores',
            'elapsed', 'client_cpu_fraction_of_2cores', 'origin_cpu_fraction', 'data_filesystem_growth_mib']}
        known = [r for r in rows if r['failed'] is not None]
        rps = [r['good_rps'] for r in rows if r['good_rps'] is not None]
        total = sum((r['successful'] or 0) + (r['failed'] or 0) for r in known)
        summary = dict(zip(['payload_bytes', 'protocol', 'cache', 'connection', 'label'], key))
        summary.update(values, samples=len(rows), passed=sum(r['status']=='passed' for r in rows),
                       known_native_samples=len(known),
                       successful=sum(r['successful'] or 0 for r in rows),
                       failed=sum(r['failed'] or 0 for r in rows),
                       error_fraction=sum(r['failed'] for r in known)/total if total and len(known)==len(rows) else None,
                       rps_min=min(rps) if rps else None, rps_max=max(rps) if rps else None,
                       quality_flags=[])
        if summary['passed'] != len(rows):
            summary['quality_flags'].append('traffic_or_state_failure')
        if summary['max_core_steal_median'] is not None and summary['max_core_steal_median'] > .1:
            summary['quality_flags'].append('high_cpu_steal')
        if rps and median(rps) > 0 and (max(rps)-min(rps))/median(rps) > .15:
            summary['quality_flags'].append('rps_spread_over_15pct')
        if summary['elapsed_median'] is not None and summary['elapsed_median'] < 1:
            summary['quality_flags'].append('short_batch_under_1s')
        if summary['client_cpu_fraction_of_2cores_median'] is not None and summary['client_cpu_fraction_of_2cores_median'] > .8:
            summary['quality_flags'].append('client_cpu_above_80pct')
        summaries.append(summary)
    args.output.mkdir(parents=True, exist_ok=True)
    result = {'runs': runs, 'cases': records, 'groups': summaries,
              'case_count': len(records), 'all_samples_retained': True,
              'long_stability_completed': False, 'index_sha256': sha(args.index)}
    (args.output / 'results.json').write_text(json.dumps(result, indent=2) + '\n')
    with (args.output / 'cases.csv').open('w', newline='') as file:
        writer = csv.DictWriter(file, fieldnames=list(records[0]))
        writer.writeheader()
        writer.writerows(records)
    print(json.dumps({'cases': len(records), 'groups': len(summaries),
                      'passed': sum(r['status']=='passed' for r in records)}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--index', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    main(parser.parse_args())
