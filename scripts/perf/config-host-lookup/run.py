#!/usr/bin/env python3
"""Compare exact configuration lookup normalization, borrowing unchanged lower-case keys."""
import argparse
from collections import defaultdict
import csv
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import textwrap

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/config-host-lookup-ci'
BASE = '1c441cea3b416f2d4316abed4ec1863e4666852f'
CASES = ['lower', 'mixed', 'port', 'tail-dots', 'userinfo', 'ipv6-bracket', 'ipv6-bare', 'unmatched', 'unicode', 'long-upper', 'long-lower', 'empty', 'spaces', 'all-dots', 'reparsed-colon', 'bracket-non-ip']
OWNED = {'mixed', 'port', 'tail-dots', 'userinfo', 'ipv6-bracket', 'ipv6-bare', 'unmatched', 'unicode', 'long-upper', 'bracket-non-ip'}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def function(source, signature):
    start = source.index(signature)
    return source[start:source.index('\n}\n', start) + 3]


def lookup_function(config):
    start = config.index('    fn normalize_host_for_lookup(')
    block = textwrap.dedent(config[start:config.index('\n    }\n', start) + 7])
    return block.replace('fn normalize_host_for_lookup(', 'fn candidate(', 1).replace(
        'crate::lb_factory::strip_addr_port_ref(', 'candidate_strip_addr_port(').strip()


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    lb = subprocess.check_output(['git', 'show', BASE + ':src/lb_factory.rs'], text=True)
    config = subprocess.check_output(['git', 'show', BASE + ':src/config.rs'], text=True)
    (source / 'original-lb-factory.rs').write_text(lb)
    (source / 'original-config.rs').write_text(config)
    strip = function(lb, 'pub(crate) fn strip_addr_port_ref(').replace(
        'pub(crate) fn strip_addr_port_ref(', 'fn candidate_strip_addr_port(', 1)
    owned_strip = function(lb, 'pub(crate) fn strip_addr_port(').replace(
        'pub(crate) fn strip_addr_port(', 'fn baseline_strip_addr_port(', 1).replace(
        'strip_addr_port_ref(', 'candidate_strip_addr_port(')
    start = config.index('    fn normalize_host(')
    normalize = textwrap.dedent(config[start:config.index('\n    }\n', start) + 7])
    normalize = normalize.replace('fn normalize_host(', 'fn baseline_owned(', 1).replace(
        'crate::lb_factory::strip_addr_port(', 'baseline_strip_addr_port(')
    (source / 'original.rs').write_text(strip + '\n' + owned_strip + '\n' + normalize +
        "\nfn baseline(host: &str) -> std::borrow::Cow<'_, str> { std::borrow::Cow::Owned(baseline_owned(host)) }\n")
    shutil.copy2(ROOT / 'benches/support/config_host_lookup_candidate.rs', source / 'candidate.rs')
    current = (ROOT / 'src/config.rs').read_text()
    integrated = '    fn normalize_host_for_lookup(' in current
    if integrated:
        assert lookup_function(current) == (source / 'candidate.rs').read_text().strip()
        (source / 'integrated-config.rs').write_text(current)
    shutil.copy2(ROOT / 'benches/support/config_host_lookup_probe.rs', source / 'main.rs')
    allocator = (ROOT / 'benches/support/sni_client_hello_probe.rs').read_text()
    (source / 'allocator.rs').write_text(allocator[allocator.index('#[cfg(feature = "count-allocations")]'):
                                                allocator.index('\nfn record(payload:')])
    manifest = (ROOT / 'scripts/perf/config-host-lookup/Cargo.toml').read_text().replace(
        '../../../benches/support/config_host_lookup_probe.rs', 'main.rs')
    (source / 'Cargo.toml').write_text(manifest)
    shutil.copy2(ROOT / 'scripts/perf/config-host-lookup/Cargo.lock', source / 'Cargo.lock')
    target = ROOT / '.local/config-host-lookup-target'
    for label, extra in [('timing', []), ('allocations', ['--features', 'count-allocations'])]:
        for command, flags in [('clippy', ['--', '-D', 'warnings']), ('build', ['--release'])]:
            with (OUT / (label + '-' + command + '.log')).open('w') as log:
                subprocess.run(['cargo', command, '--locked', '--manifest-path', str(source / 'Cargo.toml'),
                                '--target-dir', str(target), *extra, *flags], stdout=log, stderr=subprocess.STDOUT, check=True)
        shutil.copy2(target / 'release/config-host-lookup-probe', OUT / label)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'baseline': BASE, 'integrated': integrated,
                'scope': 'exact ConfigStore host lookup normalization; System allocator; not the full getter or node throughput',
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
    if metadata['integrated']:
        assert lookup_function((OUT / 'source/integrated-config.rs').read_text()) == (OUT / 'source/candidate.rs').read_text().strip()
    controls = read(OUT / 'controls.tsv')
    assert len(controls) == 65831
    assert all(int(row['case']) == i and row['baseline'] == row['candidate'] 
               for i, row in enumerate(controls))
    assert len(metadata['runs']) == 16
    assert [(r['round'], r['variant']) for r in metadata['runs']] == [(i, v) for i in range(8)
        for v in (['baseline', 'candidate'] if i % 2 == 0 else ['candidate', 'baseline'])]
    values = defaultdict(dict)
    allocations = {variant: read(OUT / (variant + '-allocations.tsv')) for variant in ['baseline', 'candidate']}
    for variant, rows in allocations.items():
        assert [row['case'] for row in rows] == CASES
        for row in rows:
            assert int(row['iterations']) == 1000 and int(row['reallocations']) == 0
            expected = 0 if row['case'] == 'empty' else (1 if row['case'] == 'all-dots' else 2)
            if variant == 'candidate':
                expected = int(row['case'] in OWNED)
            assert int(row['allocations']) == 1000 * expected, row
    for run in metadata['runs']:
        rows = read(OUT / run['file'])
        assert len(rows) == 320 and {r['case'] for r in rows} == set(CASES)
        for case in CASES:
            selected = [row for row in rows if row['case'] == case]
            assert [int(row['block']) for row in selected] == list(range(20))
            assert all(int(row['iterations']) > 0 and int(row['nanoseconds']) > 0 for row in selected)
            values[case][run['round'], run['variant']] = sum(int(row['nanoseconds']) for row in selected) / sum(int(row['iterations']) for row in selected)
    summary = {'source': metadata['source'], 'baseline': BASE, 'differential_inputs': len(controls),
        'raw_blocks': 5120,
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
