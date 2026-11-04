#!/usr/bin/env python3
"""Build and compare exact read-only header IP helpers; no node throughput claim."""
import argparse
from collections import defaultdict
import csv
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/client-ip-ci'
BASE = 'c8df43c099200aff5aefcbb2858d64fb64aa9d69'
CASES = ['public-small', 'private-no-forward3', 'private-first4', 'private-second4',
         'private-forwarded4', 'private-invalid4', 'private-priority4', 'private-large27',
         'private-large-forward28', 'mapped-private4']


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def function(source, signature):
    start = source.index(signature)
    return source[start:source.index('\n}\n', start) + 3]


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    (OUT / 'setup.json').write_text(json.dumps({'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(), 'baseline': BASE}) + '\n')
    original = subprocess.check_output(['git', 'show', BASE + ':src/client_ip.rs'], text=True)
    signatures = ['pub fn is_local_ip(', 'pub fn parse_candidate_ip(', 'pub fn canonicalize_ip(', 'pub fn fallback_client_ip(']
    header_names = original[original.index('static FALLBACK_IP_HEADERS:'):original.index('static HOSTNAME:')]
    extracted = header_names + '\n' + '\n'.join(function(original, name) for name in signatures)
    extracted = extracted.replace('pub fn fallback_client_ip(', 'fn baseline(')
    (source / 'original.rs').write_text(extracted)
    (source / 'original-full.rs').write_text(original)
    candidate = (ROOT / 'benches/support/client_ip_candidate.rs').read_text()
    tail = function(original, 'pub fn fallback_client_ip(')
    tail = tail[tail.index('    for header in FALLBACK_IP_HEADERS.iter()'):]
    assert candidate[candidate.index('    for header in FALLBACK_IP_HEADERS.iter()'):].strip() == tail.strip()
    (source / 'candidate.rs').write_text(candidate)
    shutil.copy2(ROOT / 'benches/support/client_ip_probe.rs', source / 'main.rs')
    allocator = (ROOT / 'benches/support/sni_client_hello_probe.rs').read_text()
    (source / 'allocator.rs').write_text(allocator[allocator.index('#[cfg(feature = "count-allocations")]'):
                                                allocator.index('\nfn record(payload:')])
    manifest = (ROOT / 'scripts/perf/client-ip/Cargo.toml').read_text().replace(
        '../../../benches/support/client_ip_probe.rs', 'main.rs')
    (source / 'Cargo.toml').write_text(manifest)
    shutil.copy2(ROOT / 'scripts/perf/client-ip/Cargo.lock', source / 'Cargo.lock')
    root_lock = (ROOT / 'Cargo.lock').read_text()
    standalone_lock = (source / 'Cargo.lock').read_text()
    for name in ['http', 'bytes', 'itoa']:
        pattern = r'\[\[package\]\]\nname = "' + name + r'"\n(.*?)(?=\n\[\[package\]\]|\Z)'
        assert re.search(pattern, root_lock, re.S).group(1).strip() == re.search(pattern, standalone_lock, re.S).group(1).strip()
    target = ROOT / '.local/client-ip-target'
    for label, extra in [('timing', []), ('allocations', ['--features', 'count-allocations'])]:
        with (OUT / (label + '-clippy.log')).open('w') as log:
            subprocess.run(['cargo', 'clippy', '--locked', '--manifest-path', str(source / 'Cargo.toml'),
                            '--target-dir', str(target), *extra, '--', '-D', 'warnings'],
                           stdout=log, stderr=subprocess.STDOUT, check=True)
        with (OUT / (label + '-build.log')).open('w') as log:
            subprocess.run(['cargo', 'build', '--locked', '--release', '--manifest-path', str(source / 'Cargo.toml'),
                            '--target-dir', str(target), *extra], stdout=log, stderr=subprocess.STDOUT, check=True)
        shutil.copy2(target / 'release/client-ip-probe', OUT / label)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'baseline': BASE, 'scope': 'exact IP parsers with read-only HeaderMap session view; no Pingora IO or node throughput',
                'rustc': subprocess.check_output(['rustc', '-vV'], text=True), 'rustflags': os.environ['RUSTFLAGS'],
                'binaries': {name: sha(OUT / name) for name in ['timing', 'allocations']},
                'sources': {p.name: sha(p) for p in source.iterdir()}, 'runs': []}
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')


def snapshot(cpu):
    return list(map(int, next(row for row in Path('/proc/stat').read_text().splitlines()
                             if row.startswith(f'cpu{cpu} ')).split()[1:]))


def measure():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    cpu = min(os.sched_getaffinity(0))
    metadata['cpu'] = cpu
    with (OUT / 'controls.tsv').open('w') as log:
        subprocess.run([str(OUT / 'timing'), 'verify'], stdout=log, check=True)
    for variant in ['baseline', 'candidate']:
        with (OUT / (variant + '-allocations.tsv')).open('w') as log:
            subprocess.run([str(OUT / 'allocations'), 'allocations', variant], stdout=log, check=True)
    for repeat in range(8):
        for variant in (['baseline', 'candidate'] if repeat % 2 == 0 else ['candidate', 'baseline']):
            for comm in Path('/proc').glob('[0-9]*/comm'):
                try:
                    assert comm.read_text().strip() not in ['rustc', 'mbx-rustc'], 'measurement overlaps compilation'
                except FileNotFoundError:
                    continue
            file = f'round-{repeat}-{variant}.tsv'
            before = snapshot(cpu)
            with (OUT / file).open('w') as log:
                subprocess.run(['taskset', '-c', str(cpu), str(OUT / 'timing'), 'timing', variant], stdout=log, check=True)
            after = snapshot(cpu)
            delta = [b - a for a, b in zip(before, after)]
            assert all(v >= 0 for v in delta[:8]) and sum(delta[:8]) > 0
            metadata['runs'].append({'round': repeat, 'variant': variant, 'file': file,
                'cpu_before': before, 'cpu_after': after, 'host_steal_fraction': delta[7] / sum(delta[:8])})
            (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    verify()


def read(path):
    with path.open() as file:
        return list(csv.DictReader(file, delimiter='\t'))


def verify():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    for name, value in metadata['binaries'].items():
        assert sha(OUT / name) == value
    for name, value in metadata['sources'].items():
        assert sha(OUT / 'source' / name) == value
    controls = read(OUT / 'controls.tsv')
    assert len(controls) == 75914
    assert all(int(row['case']) == i and row['baseline'] == row['candidate'] 
               for i, row in enumerate(controls))
    assert len(metadata['runs']) == 16
    assert [(r['round'], r['variant']) for r in metadata['runs']] == [(i, v) for i in range(8)
        for v in (['baseline', 'candidate'] if i % 2 == 0 else ['candidate', 'baseline'])]
    values = defaultdict(dict)
    allocations = {variant: read(OUT / (variant + '-allocations.tsv')) for variant in ['baseline', 'candidate']}
    for rows in allocations.values():
        assert [row['case'] for row in rows] == CASES
        assert all(int(r[k]) == 0 for r in rows for k in ['allocations', 'reallocations', 'bytes', 'largest'])
    for run in metadata['runs']:
        rows = read(OUT / run['file'])
        assert len(rows) == 200 and {r['case'] for r in rows} == set(CASES)
        for case in CASES:
            selected = [row for row in rows if row['case'] == case]
            assert [int(row['block']) for row in selected] == list(range(20))
            assert all(int(row['iterations']) > 0 and int(row['nanoseconds']) > 0 for row in selected)
            values[case][run['round'], run['variant']] = sum(int(row['nanoseconds']) for row in selected) / sum(int(row['iterations']) for row in selected)
    summary = {'source': metadata['source'], 'baseline': BASE, 'differential_inputs': len(controls),
        'raw_blocks': 3200,
        'max_host_steal_fraction': max(r['host_steal_fraction'] for r in metadata['runs']),
        'allocations': allocations, 'cases': {}}
    for case in CASES:
        changes = [values[case][i, 'candidate'] / values[case][i, 'baseline'] - 1 for i in range(8)]
        summary['cases'][case] = {'baseline_ns': statistics.median(values[case][i, 'baseline'] for i in range(8)),
            'candidate_ns': statistics.median(values[case][i, 'candidate'] for i in range(8)),
            'paired_changes': changes, 'paired_median': statistics.median(changes)}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'measure', 'verify'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        OUT = args.directory.resolve()
    globals()[args.mode]()
