#!/usr/bin/env python3
"""Summarize architecture diagnostics without mixing them with capacity results."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import statistics


GROUPS = {
    'request_filter': '::request_filter',
    'cache_admission': '::request_cache_filter',
    'response_filter': '::response_filter',
    'request_completion': '::logging::',
    'dimension_worker': '::record_http_dimension_event',
    'cache_lookup': 'cache_hybrid::HybridStorage',
    'memory_governor': '::memory_governor::',
    'headers': 'http::header::',
    'allocator': 'mi_',
    'upstream_peer': '::upstream_peer',
    'firewall_symbols': '::firewall::',
    'upstream_pipeline_including_callbacks': '::proxy_to_upstream',
    'zstd': 'ZSTD_',
    'zstd_context_creation': 'ZSTD_createCCtx',
    'tokio_filesystem': 'tokio::fs::',
    'file_fill_handler': 'FileMissHandler',
    'metadata_writer': 'cache_meta_writer',
    'heap_relinquish': 'relinquish_thread_heap',
    'bloom_symbols': 'bloom_',
    'parse_mark_insert': 'insert_http_request_parse_mark',
    'client_ip_header_lookup': 'client_ip::header_value_ci',
    'request_id': 'logging::next_request_id',
    'response_transform_probe': 'request_may_change_response_representation',
}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def profile(path):
    blocks = [block for block in path.read_text().split('\n\n') if 'cpu-clock:u:' in block]
    if not blocks:
        raise ValueError('empty CPU profile')
    samples = [re.sub(r'\[[0-9a-f]{8,}\]', '', block) for block in blocks]
    periods = Counter(int(re.search(r'(\d+) cpu-clock:u:', b).group(1)) for b in samples)
    groups = {name: {'samples': sum(match in block for block in samples)}
              for name, match in GROUPS.items()}
    for group in groups.values():
        group['percent'] = group['samples'] * 100 / len(samples)
    leaves = Counter()
    for block in samples:
        frame = block.splitlines()[1].strip().split(' ', 1)[1]
        leaves[re.sub(r'\+0x[0-9a-f]+.*', '', frame)] += 1
    flat = Path(str(path).replace('.stacks.txt', '.perf-flat.txt')).read_text()
    lost = re.search(r'Total Lost Samples:\s*(\d+)', flat)
    if not lost or int(lost.group(1)):
        raise ValueError('lost samples or missing loss counter')
    return {'path': str(path), 'sha256': sha(path), 'event': 'cpu-clock:u',
            'sample_count': len(samples), 'lost_samples': int(lost.group(1)),
            'periods': dict(periods), 'inclusive_groups_overlap': True,
            'groups': groups, 'leaf_symbols': [
                {'symbol': symbol, 'samples': count, 'percent': count * 100 / len(samples)}
                for symbol, count in leaves.most_common(50)]}


def trace(path, successful):
    counts = {}
    for line in path.read_text().splitlines():
        parts = line.split()
        if len(parts) not in (5, 6) or not parts[0][0].isdigit():
            continue
        counts[parts[-1]] = {'calls': int(parts[3]),
                             'errors': int(parts[4]) if len(parts) == 6 else 0}
    total = counts.pop('total')['calls']
    if sum(v['calls'] for v in counts.values()) != total:
        raise ValueError('syscall totals do not reconcile')
    return {'path': str(path), 'sha256': sha(path), 'successful': successful,
            'total_calls': total, 'calls_per_200': total / successful,
            'syscalls': {key: {**value, 'calls_per_200': value['calls'] / successful}
                         for key, value in counts.items()},
            'syscall_errors_are_not_http_errors': True,
            'strace_times_are_not_cpu_time': True}


def main(args):
    result = {'cpu_profiles': [], 'traces': [], 'capacity_cpu': [],
              'diagnostic_rps_excluded': True,
              'capacity_source_sha256': sha(args.capacity / 'results.json')}
    root = Path(__file__).resolve().parents[2]
    build = json.loads((args.capacity / 'build-metadata.json').read_text())
    for path, digest in build['source_sha256'].items():
        if sha(root / path) != digest:
            raise ValueError('source changed since measured binary: ' + path)
    result['production_source_matches_capacity_binary_metadata'] = True
    for run in args.profiles:
        manifest = json.loads((run / 'manifest.json').read_text())
        if manifest['status'] != 'completed' or manifest.get('cleanup_errors'):
            raise ValueError('incomplete profile fixture')
        for binary, digest in manifest['binaries'].items():
            if sha(Path(binary)) != digest:
                raise ValueError('profile binary changed')
        for case in manifest['cases']:
            if case['status'] != 'passed' or case['native']['failed']:
                raise ValueError('invalid instrumented workload')
            entry = profile(run / (case['stem'] + '.stacks.txt'))
            entry.update(state=case['cache'], successful=case['native']['successful'],
                         failed=case['native']['failed'], manifest_sha256=sha(run / 'manifest.json'))
            result['cpu_profiles'].append(entry)
    for run in args.traces:
        manifest = json.loads((run / 'manifest.json').read_text())
        if manifest['status'] != 'completed' or manifest.get('cleanup_errors'):
            raise ValueError('incomplete trace fixture')
        for case in manifest['cases']:
            if case['status'] != 'passed' or case['native']['failed']:
                raise ValueError('invalid traced workload')
            entry = trace(run / (case['stem'] + '.strace.txt'), case['native']['successful'])
            entry.update(target=manifest['target'], state=case['cache'],
                         manifest_sha256=sha(run / 'manifest.json'))
            result['traces'].append(entry)
    capacity = json.loads((args.capacity / 'results.json').read_text())
    for run in capacity['runs']:
        manifest = json.loads(Path(run['manifest']).read_text())
        if sha(Path(run['manifest'])) != run['manifest_sha256']:
            raise ValueError('capacity manifest changed')
        if manifest['args']['payload_size'] != 1024:
            continue
        for state in ['hit', 'off', 'miss']:
            cases = [c for c in manifest['cases'] if c['protocol'] == 'h1'
                     and c['cache'] == state and c['connection'] == 'reuse'
                     and c['status'] == 'passed']
            values = [{key: (c['proxy_after']['cpu.stat'][key] - c['proxy_before']['cpu.stat'][key])
                       / c['native']['successful'] for key in ['usage_usec', 'user_usec', 'system_usec']}
                      for c in cases]
            result['capacity_cpu'].append({'target': manifest['target'], 'state': state,
                'samples': len(values), 'independent_medians_not_additive': True,
                **{key + '_per_200_median': statistics.median(v[key] for v in values)
                   for key in values[0]}})
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--capacity', type=Path, required=True)
    parser.add_argument('--profiles', type=Path, nargs='+', required=True)
    parser.add_argument('--traces', type=Path, nargs='+', default=[])
    parser.add_argument('--output', type=Path, required=True)
    main(parser.parse_args())
