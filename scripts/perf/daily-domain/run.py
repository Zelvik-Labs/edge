#!/usr/bin/env python3
"""Compare daily map ownership and lazy capacity; governor/drop sink are fixtures."""
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
OUT = ROOT / '.local/daily-domain-ci'
BASE = 'e866d8bc52145f71d5bab641a18f33fe5afaab18'
CASES = ['hot-1', 'hot-16', 'hot-1024', 'at-cap', 'reject-new', 'insert-new', 'invalid', 'legacy-hot']


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    (OUT / 'setup.json').write_text(json.dumps({
        'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'baseline': BASE, 'scope': 'setup provenance; not a completed measurement'}, indent=2) + '\n')
    source = OUT / 'source'
    source.mkdir()
    baseline = subprocess.check_output(['git', 'show', BASE + ':src/metrics/daily.rs'], text=True)
    (source / 'original-full.rs').write_text(baseline)
    original = baseline[:baseline.index('pub static DAILY_DOMAIN_TRACKER:')]
    original = original.replace('use std::sync::Arc;\n', '').replace('use std::sync::LazyLock as Lazy;\n', '')
    (source / 'baseline.rs').write_text(original)
    candidate = (ROOT / 'benches/support/daily_domain_candidate.rs').read_text()
    (source / 'candidate.rs').write_text(candidate)
    allocator = (ROOT / 'benches/support/sni_client_hello_probe.rs').read_text()
    (source / 'allocator.rs').write_text(allocator[allocator.index('#[cfg(feature = "count-allocations")]'):allocator.index('\nfn record(payload:')])
    shutil.copy2(ROOT / 'benches/support/daily_domain_probe.rs', source / 'main.rs')
    manifest = (ROOT / 'scripts/perf/daily-domain/Cargo.toml').read_text().replace('../../../benches/support/daily_domain_probe.rs', 'main.rs')
    (source / 'Cargo.toml').write_text(manifest)
    shutil.copy2(ROOT / 'scripts/perf/daily-domain/Cargo.lock', source / 'Cargo.lock')
    root_lock = (ROOT / 'Cargo.lock').read_text()
    (source / 'root-Cargo.lock').write_text(root_lock)
    pattern = r'\[\[package\]\]\nname = "([^"]+)"\n((?:[^\n]+\n){3})'
    root_packages = set(re.findall(pattern, root_lock))
    for package in re.findall(pattern, (source / 'Cargo.lock').read_text()):
        if package[0] == 'daily-domain-probe':
            continue
        assert package in root_packages, package
    target = ROOT / '.local/daily-domain-target'
    for mode, features in [('timing', []), ('allocations', ['--features', 'count-allocations'])]:
        for operation, options in [('clippy', ['--', '-D', 'warnings']), ('build', ['--release'])]:
            with (OUT / (mode + '-' + operation + '.log')).open('w') as log:
                subprocess.run(['cargo', operation, '--locked', '--manifest-path', str(source / 'Cargo.toml'),
                                '--target-dir', str(target), *features, *options], stdout=log, stderr=subprocess.STDOUT, check=True)
        shutil.copy2(target / 'release/daily-domain-probe', OUT / mode)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'baseline': BASE, 'scope': 'exact daily map helpers; fixed-capacity governor and counted drop sink fixtures; System allocator; no actual governor CPU or node throughput claim',
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
                    assert comm.read_text().strip() not in ['rustc', 'mbx-rustc'], 'overlapping compiler'
                except FileNotFoundError:
                    continue
            file = f'round-{repeat}-{variant}.tsv'
            before = snapshot(cpu)
            with (OUT / file).open('w') as log:
                subprocess.run(['taskset', '-c', str(cpu), str(OUT / 'timing'), 'timing', variant], stdout=log, stderr=subprocess.STDOUT, check=True)
            after = snapshot(cpu)
            delta = [b - a for a, b in zip(before, after)]
            assert all(v >= 0 for v in delta[:8]) and sum(delta[:8]) > 0
            metadata['runs'].append({'round': repeat, 'variant': variant, 'file': file,
                                    'cpu_before': before, 'cpu_after': after,
                                    'host_steal_fraction': delta[7] / sum(delta[:8])})
            (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    verify()


def read(path):
    with path.open() as stream:
        return list(csv.DictReader(stream, delimiter='\t'))


def verify():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    for name, expected in metadata['binaries'].items():
        assert sha(OUT / name) == expected
    for name, expected in metadata['sources'].items():
        assert sha(OUT / 'source' / name) == expected
    controls = read(OUT / 'controls.tsv')
    assert len(controls) == 20566 and [int(r['case']) for r in controls] == list(range(20566))
    assert all(r['status'] == 'PASS' and r['baseline'] == r['candidate'] for r in controls)
    allocations = {}
    for variant in ['baseline', 'candidate']:
        rows = read(OUT / (variant + '-allocations.tsv'))
        assert [r['case'] for r in rows] == CASES
        allocations[variant] = {r['case']: {k: int(v) for k, v in r.items() if k != 'case'} for r in rows}
        assert all(r['iterations'] == 1000 for r in allocations[variant].values())
    for case in ['hot-1', 'hot-16', 'hot-1024', 'at-cap', 'reject-new']:
        old, new = [allocations[v][case] for v in ['baseline', 'candidate']]
        assert old['allocations'] == 1000 and new['allocations'] == 0
        assert old['capacity_calls'] == 1000
        assert new['capacity_calls'] == (1000 if case == 'reject-new' else 0)
    assert allocations['baseline']['invalid']['capacity_calls'] == 1000
    assert allocations['candidate']['invalid']['capacity_calls'] == 0
    assert all(allocations[v]['invalid']['allocations'] == 0 for v in ['baseline', 'candidate'])
    assert all(allocations[v]['legacy-hot']['allocations'] == 1000 for v in ['baseline', 'candidate'])
    assert [(r['round'], r['variant']) for r in metadata['runs']] == [
        (i, v) for i in range(8) for v in (['baseline', 'candidate'] if i % 2 == 0 else ['candidate', 'baseline'])]
    values = defaultdict(dict)
    for run in metadata['runs']:
        rows = read(OUT / run['file'])
        assert len(rows) == 160
        for case in CASES:
            selected = [r for r in rows if r['case'] == case]
            assert len(selected) == 20 and [int(r['block']) for r in selected] == list(range(20))
            assert all(int(r['iterations']) > 0 and int(r['nanoseconds']) > 0 for r in selected)
            values[case][run['round'], run['variant']] = sum(int(r['nanoseconds']) for r in selected) / sum(int(r['iterations']) for r in selected)
    summary = {'source': metadata['source'], 'baseline': BASE, 'scope': metadata['scope'],
               'differential_controls': len(controls), 'raw_blocks': 2560, 'allocations': allocations, 'cases': {},
               'max_host_steal_fraction': max(r['host_steal_fraction'] for r in metadata['runs'])}
    for case in CASES:
        changes = [(values[case][i, 'candidate'] / values[case][i, 'baseline'] - 1) * 100 for i in range(8)]
        summary['cases'][case] = {'baseline_ns': statistics.median(values[case][i, 'baseline'] for i in range(8)),
                                 'candidate_ns': statistics.median(values[case][i, 'candidate'] for i in range(8)),
                                 'paired_percent_changes': changes, 'paired_median_percent': statistics.median(changes)}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k not in ['allocations']}, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'measure', 'verify'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        OUT = args.directory.resolve()
    globals()[args.mode]()
