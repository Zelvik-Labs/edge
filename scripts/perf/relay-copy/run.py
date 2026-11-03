#!/usr/bin/env python3
"""Compare exact production copy loops with fixed policy and metric fixtures."""
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

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/relay-copy-ci'
BASE_REF = 'b4d635519973b2ec9aa24a533129fdbb86d3fc4f'
CASES = ['1KiB_full', '64KiB_full', '1MiB_full', '64KiB_short64']


def digest(path):
    sha = hashlib.sha256()
    with path.open('rb') as source:
        while block := source.read(1024 * 1024):
            sha.update(block)
    return sha.hexdigest()


def function(source, signature):
    start = source.index(signature)
    end = source.index('\n}\n', start) + 3
    return source[start:end]


def source_inputs():
    baseline = subprocess.check_output(['git', 'show', BASE_REF + ':src/tcp_proxy.rs'], cwd=ROOT, text=True)
    candidate = (ROOT / 'src/tcp_proxy.rs').read_text()
    variants = {}
    for label, source in [('baseline', baseline), ('candidate', candidate)]:
        code = function(source, 'async fn copy_stream_and_count<')
        for signature in ['fn direction_error_or_benign(', 'fn is_benign_relay_io_error(', 'fn is_benign_relay_raw_os_error(']:
            code += '\n' + function(source, signature)
        if label == 'candidate':
            start = source.index('struct RelayProgress<')
            end = source.index('#[cfg(target_os = "linux")]\nmod zero_copy {', start)
            code += '\n' + source[start:end]
        variants[label] = {'original': source, 'production': code}
    assert 'writer.write_all' in variants['baseline']['production']
    assert 'progress.transferred(written)' in variants['candidate']['production']
    return variants


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    target = ROOT / '.local/relay-copy-target'
    template = (ROOT / 'scripts/perf/relay-copy/Cargo.toml').read_text()
    allocator_source = (ROOT / 'benches/support/sni_client_hello_probe.rs').read_text()
    allocator_start = allocator_source.index('#[cfg(feature = "count-allocations")]')
    allocator_end = allocator_source.index('\nfn record(payload:', allocator_start)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                'baseline': BASE_REF, 'rustc': subprocess.check_output(['rustc', '-vV'], text=True),
                'scope': 'exact one-direction copy loops; fixed 64KiB buffer, pressure off, atomic metric fixture, in-memory I/O; not Node throughput',
                'variants': {}}
    for label, source in source_inputs().items():
        directory = OUT / label
        (directory / 'src').mkdir(parents=True)
        (directory / 'original.rs').write_text(source['original'])
        (directory / 'src/production.rs').write_text(source['production'])
        (directory / 'src/allocator.rs').write_text(allocator_source[allocator_start:allocator_end])
        shutil.copy2(ROOT / 'benches/support/relay_copy_probe.rs', directory / 'src/main.rs')
        counter = ('type Counter = std::sync::Arc<AtomicU64>;\n'
                   'fn new_counter() -> Counter { std::sync::Arc::new(AtomicU64::new(0)) }\n'
                   'fn counter_arg(counter: &Counter) -> Counter { counter.clone() }\n') if label == 'baseline' else (
                   'type Counter = AtomicU64;\n'
                   'fn new_counter() -> Counter { AtomicU64::new(0) }\n'
                   'fn counter_arg(counter: &Counter) -> &Counter { counter }\n')
        (directory / 'src/counter.rs').write_text(counter)
        manifest = template.replace('name = "relay-copy-probe"', f'name = "relay-{label}"').replace(
            'path = "../../../benches/support/relay_copy_probe.rs"', 'path = "src/main.rs"')
        (directory / 'Cargo.toml').write_text(manifest)
        shutil.copy2(ROOT / 'scripts/perf/relay-copy/Cargo.lock', directory / 'Cargo.lock')
        binaries = {}
        for mode, extra in [('timing', []), ('allocations', ['--features', 'count-allocations'])]:
            command = ['cargo', 'build', '--locked', '--release', '--manifest-path', str(directory / 'Cargo.toml'),
                       '--target-dir', str(target), *extra]
            with (directory / (mode + '-build.log')).open('w') as log:
                subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
            shutil.copy2(target / 'release' / ('relay-' + label), directory / mode)
            binaries[mode] = digest(directory / mode)
        metadata['variants'][label] = {'binaries': binaries,
            'sources': {str(path.relative_to(directory)): digest(path) for path in directory.rglob('*')
                        if path.is_file() and path.suffix in ['.rs', '.toml', '.lock']}}
    (OUT / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')


def snapshot(cpu):
    return [int(value) for value in next(row for row in Path('/proc/stat').read_text().splitlines()
                                        if row.startswith(f'cpu{cpu} ')).split()[1:]]


def measure():
    metadata = json.loads((OUT / 'build-metadata.json').read_text())
    cpu = min(os.sched_getaffinity(0))
    metadata.update({'cpu': cpu, 'runs': []})
    for label in ['baseline', 'candidate']:
        directory = OUT / label
        for mode in ['controls', 'assert-accounting', 'allocations']:
            binary = directory / ('allocations' if mode == 'allocations' else 'timing')
            with (directory / (mode + '.log')).open('w') as log:
                result = subprocess.run(['taskset', '-c', str(cpu), str(binary), '--' + mode], stdout=log, stderr=subprocess.STDOUT)
            expected = 101 if label == 'baseline' and mode == 'assert-accounting' else 0
            assert result.returncode == expected, (label, mode, result.returncode)
            if expected:
                assert 'accepted partial writes must survive error or cancellation' in (directory / (mode + '.log')).read_text()
    for repeat in range(5):
        for label in (['baseline', 'candidate'] if repeat % 2 == 0 else ['candidate', 'baseline']):
            file = f'round-{repeat}-{label}.tsv'
            before = snapshot(cpu)
            with (OUT / file).open('w') as log:
                subprocess.run(['taskset', '-c', str(cpu), str(OUT / label / 'timing'), '--timing'],
                               stdout=log, stderr=subprocess.STDOUT, check=True)
            after = snapshot(cpu)
            delta = [b - a for a, b in zip(before, after)]
            assert all(value >= 0 for value in delta[:8]) and sum(delta[:8]) > 0
            metadata['runs'].append({'round': repeat, 'variant': label, 'file': file,
                                    'cpu_before': before, 'cpu_after': after,
                                    'host_steal_fraction': delta[7] / sum(delta[:8])})
            (OUT / 'measurement.json').write_text(json.dumps(metadata, indent=2) + '\n')
    verify()


def read_tsv(path):
    with path.open() as source:
        return list(csv.DictReader(source, delimiter='\t'))


def verify():
    metadata = json.loads((OUT / 'measurement.json').read_text())
    assert len(metadata['runs']) == 10
    allocations, controls = {}, {}
    for label in ['baseline', 'candidate']:
        for file, sha in metadata['variants'][label]['binaries'].items():
            assert digest(OUT / label / file) == sha
        for file, sha in metadata['variants'][label]['sources'].items():
            assert digest(OUT / label / file) == sha
        allocations[label] = {row['case']: {k: int(v) for k, v in row.items() if k != 'case'}
                              for row in read_tsv(OUT / label / 'allocations.log')}
        controls[label] = {row['case']: {k: int(v) for k, v in row.items() if k != 'case'}
                           for row in read_tsv(OUT / label / 'controls.log')}
        assert len(controls[label]) == 4
    for case in CASES:
        assert allocations['baseline'][case]['allocations'] - allocations['candidate'][case]['allocations'] == 1
        assert allocations['baseline'][case]['reallocations'] == allocations['candidate'][case]['reallocations'] == 0
    for case, values in controls['candidate'].items():
        assert values['accepted'] == values['counter'] == values['flushed']
        assert controls['baseline'][case]['flushed'] == 0
    values = defaultdict(dict)
    for run in metadata['runs']:
        rows = read_tsv(OUT / run['file'])
        assert len(rows) == 80
        for case in CASES:
            selected = [row for row in rows if row['case'] == case]
            assert len(selected) == 20 and [int(row['block']) for row in selected] == list(range(20))
            iterations = sum(int(row['iterations']) for row in selected)
            elapsed = sum(int(row['nanoseconds']) for row in selected)
            assert iterations > 0 and elapsed > 0
            values[case][run['round'], run['variant']] = elapsed / iterations
    summary = {'source': metadata['source'], 'baseline': metadata['baseline'], 'allocations': allocations,
               'controls': controls, 'cases': {}, 'max_host_steal_fraction': max(run['host_steal_fraction'] for run in metadata['runs'])}
    for case in CASES:
        paired = [values[case][r, 'candidate'] / values[case][r, 'baseline'] - 1 for r in range(5)]
        summary['cases'][case] = {'baseline_ns': statistics.median(values[case][r, 'baseline'] for r in range(5)),
                                 'candidate_ns': statistics.median(values[case][r, 'candidate'] for r in range(5)),
                                 'paired_change': paired, 'paired_median': statistics.median(paired)}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'measure', 'verify', 'check-source'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        OUT = args.directory.resolve()
    if args.mode == 'check-source':
        print(json.dumps({label: len(value['production']) for label, value in source_inputs().items()}))
    else:
        globals()[args.mode]()
