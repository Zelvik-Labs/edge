#!/usr/bin/env python3
"""Build exact parser controls, verify their outputs, then measure separately."""
import argparse
from collections import defaultdict
import csv
import hashlib
import json
import itertools
import os
from pathlib import Path
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/sni-client-hello-ci'
BASE_REF = 'ac46a48becb18e1c36309eee541bda3d0c6b7b93'
BOUNDED_REF = 'b4d635519973b2ec9aa24a533129fdbb86d3fc4f'
VARIANTS = ['baseline', 'bounded', 'candidate']


def digest(path):
    sha = hashlib.sha256()
    with path.open('rb') as source:
        while block := source.read(1024 * 1024):
            sha.update(block)
    return sha.hexdigest()


def source_inputs():
    revision = subprocess.check_output(['git', 'rev-parse', BASE_REF], cwd=ROOT, text=True).strip()
    old = subprocess.check_output(['git', 'show', f'{revision}:src/http_proxy_manager.rs'], cwd=ROOT, text=True)
    enum_start = old.index('#[derive(Debug)]\nenum ClientHelloParse {')
    enum_end = old.index('\n}\n', enum_start) + 3
    fn_start = old.index('fn parse_tls_client_hello_sni(')
    fn_end = old.index('\n}\n', fn_start) + 3
    baseline = (old[enum_start:enum_end].replace('enum ClientHelloParse', 'pub(crate) enum ClientHelloParse') + '\n' +
                old[fn_start:fn_end].replace('fn parse_tls_client_hello_sni(', 'pub(crate) fn parse_tls_client_hello_sni(', 1))
    bounded = subprocess.check_output(['git', 'show', BOUNDED_REF + ':src/tls_client_hello.rs'], cwd=ROOT, text=True)
    return revision, old, baseline, bounded, (ROOT / 'src/tls_client_hello.rs').read_text()


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    revision, old, baseline, bounded, candidate = source_inputs()
    (OUT / 'baseline-original.rs').write_text(old)
    metadata = {
        'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'baseline': revision,
        'bounded': BOUNDED_REF,
        'rustc': subprocess.check_output(['rustc', '-vV'], text=True),
        'baseline_original_sha256': digest(OUT / 'baseline-original.rs'),
        'variants': {},
        'scope': 'pure production SNI parser, System allocator; no full-node throughput claim',
    }
    for variant, source in [('baseline', baseline), ('bounded', bounded), ('candidate', candidate)]:
        directory = OUT / variant
        directory.mkdir()
        (directory / 'parser.rs').write_text(source)
        shutil.copy2(ROOT / 'benches/support/sni_client_hello_probe.rs', directory / 'probe.rs')
        binaries = {}
        for label, extra in [('timing', []), ('allocations', ['--cfg', 'feature="count-allocations"'])]:
            command = ['rustc', '--edition=2024', '-D', 'warnings', '-C', 'opt-level=3', '-C', 'target-cpu=x86-64-v2',
                       *extra, str(directory / 'probe.rs'), '-o', str(directory / label)]
            with (directory / (label + '-build.log')).open('w') as log:
                subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
            binaries[label] = digest(directory / label)
        metadata['variants'][variant] = {'parser_sha256': digest(directory / 'parser.rs'),
                                         'probe_sha256': digest(directory / 'probe.rs'), 'binaries': binaries}
    with (OUT / 'tests-build.log').open('w') as log:
        subprocess.run(['rustc', '--edition=2024', '--test', '-D', 'warnings', '-C', 'opt-level=2',
                        '-C', 'target-cpu=x86-64-v2', str(OUT / 'candidate/parser.rs'), '-o', str(OUT / 'parser-tests')],
                       stdout=log, stderr=subprocess.STDOUT, check=True)
    with (OUT / 'tests.log').open('w') as log:
        subprocess.run([str(OUT / 'parser-tests'), '--test-threads=1', '--nocapture'], stdout=log, stderr=subprocess.STDOUT, check=True)
    (OUT / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')


def snapshot(cpu):
    rows = Path('/proc/stat').read_text().splitlines()
    return [int(value) for value in next(line for line in rows if line.startswith(f'cpu{cpu} ')).split()[1:]]


def measure():
    metadata = json.loads((OUT / 'build-metadata.json').read_text())
    cpu = min(os.sched_getaffinity(0))
    metadata['cpu'] = cpu
    metadata['runs'] = []
    for variant in VARIANTS:
        directory = OUT / variant
        for label, sha in metadata['variants'][variant]['binaries'].items():
            assert digest(directory / label) == sha
        for mode in ['corpus', 'allocations', 'assert-bounded']:
            binary = directory / ('timing' if mode == 'corpus' else 'allocations')
            with (directory / (mode + '.log')).open('w') as log:
                result = subprocess.run(['taskset', '-c', str(cpu), str(binary), '--' + mode], stdout=log, stderr=subprocess.STDOUT)
            expected = 101 if variant == 'baseline' and mode == 'assert-bounded' else 0
            assert result.returncode == expected, (variant, mode, result.returncode)
            if expected:
                assert 'incomplete untrusted length must not allocate' in (directory / (mode + '.log')).read_text()
    assert (OUT / 'baseline/corpus.log').read_bytes() == (OUT / 'bounded/corpus.log').read_bytes() == (OUT / 'candidate/corpus.log').read_bytes()
    metadata['corpus_cases'] = len((OUT / 'candidate/corpus.log').read_text().splitlines())
    metadata['corpus_sha256'] = digest(OUT / 'candidate/corpus.log')
    for repeat, order in enumerate(itertools.permutations(VARIANTS)):
        for variant in order:
            file = f'round-{repeat}-{variant}.tsv'
            before = snapshot(cpu)
            with (OUT / file).open('w') as log:
                subprocess.run(['taskset', '-c', str(cpu), str(OUT / variant / 'timing'), '--timing'],
                               stdout=log, stderr=subprocess.STDOUT, check=True)
            after = snapshot(cpu)
            delta = [b - a for a, b in zip(before, after)]
            assert all(value >= 0 for value in delta[:8]) and sum(delta[:8]) > 0
            metadata['runs'].append({'round': repeat, 'variant': variant, 'file': file,
                                     'cpu_before': before, 'cpu_after': after,
                                     'host_steal_fraction': delta[7] / sum(delta[:8])})
            (OUT / 'measurement.json').write_text(json.dumps(metadata, indent=2) + '\n')
    for variant in VARIANTS:
        for label, sha in metadata['variants'][variant]['binaries'].items():
            assert digest(OUT / variant / label) == sha
    verify()


def verify():
    metadata = json.loads((OUT / 'measurement.json').read_text())
    assert len(metadata['runs']) == 18
    for variant in VARIANTS:
        assert digest(OUT / variant / 'corpus.log') == metadata['corpus_sha256']
        for file, sha in metadata['variants'][variant]['binaries'].items():
            assert digest(OUT / variant / file) == sha
    cases = ['single', 'fragmented', 'partial', 'oversized', 'trailing']
    values = defaultdict(dict)
    for run in metadata['runs']:
        with (OUT / run['file']).open() as source:
            rows = list(csv.DictReader(source, delimiter='\t'))
        assert len(rows) == 100
        for case in cases:
            selected = [row for row in rows if row['case'] == case]
            assert len(selected) == 20 and [int(row['block']) for row in selected] == list(range(20))
            iterations = sum(int(row['iterations']) for row in selected)
            elapsed = sum(int(row['nanoseconds']) for row in selected)
            assert iterations > 0 and elapsed > 0
            values[case][run['round'], run['variant']] = elapsed / iterations
    allocations = {}
    for variant in VARIANTS:
        with (OUT / variant / 'allocations.log').open() as source:
            allocations[variant] = {row['case']: {k: int(v) for k, v in row.items() if k != 'case'}
                                    for row in csv.DictReader(source, delimiter='\t')}
    assert allocations['baseline']['oversized']['max_allocation_bytes'] == 0x1000003
    for variant in ['bounded', 'candidate']:
        assert all(value == 0 for value in allocations[variant]['oversized'].values())
        assert all(value == 0 for value in allocations[variant]['partial'].values())
    for case in ['single', 'fragmented', 'trailing']:
        assert allocations['candidate'][case]['allocations'] == 1
        assert allocations['candidate'][case]['reallocations'] == 0
        assert allocations['candidate'][case]['requested_bytes'] == 11
    summary = {'source': metadata['source'], 'baseline': metadata['baseline'], 'bounded': metadata['bounded'],
               'corpus_cases': metadata['corpus_cases'], 'allocations': allocations, 'cases': {},
               'max_host_steal_fraction': max(run['host_steal_fraction'] for run in metadata['runs'])}
    for case in cases:
        row = {variant + '_ns': statistics.median(values[case][r, variant] for r in range(6)) for variant in VARIANTS}
        for reference in ['baseline', 'bounded']:
            paired = [values[case][r, 'candidate'] / values[case][r, reference] - 1 for r in range(6)]
            row['paired_change_vs_' + reference] = paired
            row['paired_median_vs_' + reference] = statistics.median(paired)
        summary['cases'][case] = row
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'measure', 'verify', 'check-source'])
    args = parser.parse_args()
    if args.mode == 'check-source':
        revision, old, baseline, bounded, candidate = source_inputs()
        assert 'Vec::with_capacity(needed_handshake_len)' in baseline and 'let mut available = 0usize' in bounded
        assert 'RecordPayload' in candidate and 'Vec::with_capacity(needed_handshake_len)' not in candidate
        print(json.dumps({'baseline': revision,
        'bounded': BOUNDED_REF, 'baseline_bytes': len(baseline), 'bounded_bytes': len(bounded), 'candidate_bytes': len(candidate)}))
    else:
        globals()[args.mode]()
