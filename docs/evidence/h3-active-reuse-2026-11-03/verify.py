#!/usr/bin/env python3
"""Verify retained archives, real H3 waves and paired arithmetic; no compilation."""
import hashlib
import itertools
import json
from pathlib import Path
import statistics
import sys
import zipfile

root = Path(__file__).parent
for manifest in root.glob('*/manifest.json'):
    metadata = json.loads(manifest.read_text())
    archive = manifest.with_name('raw.zip')
    assert archive.stat().st_size == metadata['archive_bytes']
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == metadata['archive_sha256']
    with zipfile.ZipFile(archive) as source:
        for item in metadata['files']:
            value = source.read(item['path'])
            assert len(value) == item['bytes']
            assert hashlib.sha256(value).hexdigest() == item['sha256']

with zipfile.ZipFile(root/'paired/raw.zip') as source:
    metadata = json.loads(source.read('metadata.json'))
    rows = json.loads(source.read('records.json'))
    assert len(rows) == 10
    ratios, steals = [], []
    for repeat in range(5):
        pair = {}
        for mode in ['off', 'on']:
            label = f'round-{repeat}-{mode}'
            raw = json.loads(source.read(label+'.json'))
            assert raw['passed'] and raw['active_reuse'] == (mode == 'on')
            assert raw['measured_requests'] == 3200 and raw['warmup_requests'] == 1
            assert raw['concurrency'] == 32
            assert raw['accepted_and_closed_connections'] == (1 if mode == 'on' else 3101)
            assert len(raw['wave_ns']) == 100 and min(raw['wave_ns']) > 0
            value = sum(raw['wave_ns']) / 3200
            row = next(r for r in rows if r['round'] == repeat and r['mode'] == mode)
            assert row['ns_per_request'] == value
            pair[mode] = value
            host = json.loads(source.read(label+'-host.json'))
            assert host['exit_code'] == 0
            def parse(text):
                return {r.split()[0]: list(map(int, r.split()[1:])) for r in text.splitlines() if r.startswith('cpu')}
            cpu = 'cpu'+str(metadata['pinned_cpu'])
            a, b = (parse(host[part]['proc_stat'])[cpu] for part in ['before', 'after'])
            delta = [y-x for x, y in zip(a, b)]
            assert all(v >= 0 for v in delta) and sum(delta[:8]) > 0
            steals.append(delta[7]/sum(delta[:8]))
        ratios.append(pair['on']/pair['off'])
    assert ratios == json.loads(source.read('summary.json'))['paired_ratios']
    assert '27 passed; 0 failed; 2 ignored' in source.read('regressions.log').decode()

result = json.loads((root/'verified-summary.json').read_text())
assert result['source'] == metadata['commit']
assert result['binary_sha256'] == metadata['binary_sha256']
assert result['paired_ratios'] == ratios
assert result['paired_median_delta_pct'] == 100*(statistics.median(ratios)-1)
assert result['host_window_cpu_steal'] == steals
for mode in ['off', 'on']:
    assert result[mode+'_median_ns_per_request'] == statistics.median(r['ns_per_request'] for r in rows if r['mode'] == mode)
if len(sys.argv) == 2:
    assert hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest() == metadata['binary_sha256']
    print('Actual benchmark executable SHA-256 verified')
with zipfile.ZipFile(root/'native-regressions/raw.zip') as source:
    expected = {'-'.join(v) for v in itertools.product(['copied', 'owned'], ['eager', 'cooperative'], ['full', 'bridge'], ['1', '2'], ['worker', 'readable'])}
    for label in expected:
        raw = json.loads(source.read(label+'.json'))
        assert raw['passed'] and raw['exit_code'] == 0 and raw['network_cleanup_ok']
        assert raw['remaining_namespaces'] == ''
        assert len(raw['remaining_links']) == 1 and raw['remaining_links'][0]['ifname'] == 'lo'
        assert b'xdp-smoke: smoke completed' in source.read(label+'.log')
print('Archives, 10 processes, 1,000 waves, paired arithmetic and 32 native functional permutations verified')
