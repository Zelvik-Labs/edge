#!/usr/bin/env python3
"""Check timestamp ABA, Loom publication and the cost of the candidate protocol."""
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
OUT = ROOT / '.local/snapshot-sequence-ci'
CASES = ['read-2', 'read-20', 'publish-every-1024', 'publish-every-16']


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(cpu):
    return list(map(int, next(r for r in Path('/proc/stat').read_text().splitlines()
                             if r.startswith(f'cpu{cpu} ')).split()[1:]))


def run():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    for name in ['snapshot_sequence_candidate.rs', 'snapshot_sequence_probe.rs', 'snapshot_sequence_model.rs']:
        shutil.copy2(ROOT / 'benches/support' / name, source / name)
    original = subprocess.check_output(['git', 'show', 'fa66baeda0486ae8c993f9591138855c23600e86:src/memory_governor.rs'], text=True)
    (source / 'original-memory-governor.rs').write_text(original)
    assert 'if self.cached_at_millis.load(Ordering::Acquire) as i64 == cached_at' in original
    assert 'self.cached_at_millis.store(0, Ordering::Relaxed)' in original
    assert 'self.cached_at_millis.store(now as u64, Ordering::Relaxed)' in original
    manifest = (ROOT / 'scripts/perf/snapshot-sequence/Cargo.toml').read_text().replace('../../../benches/support/', '')
    (source / 'Cargo.toml').write_text(manifest)
    shutil.copy2(ROOT / 'scripts/perf/snapshot-sequence/Cargo.lock', source / 'Cargo.lock')
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'baseline': 'fa66baeda0486ae8c993f9591138855c23600e86',
                'scope': 'isolated atomic publication protocol; not full governor cost, production integration or node throughput',
                'rustflags': os.environ['RUSTFLAGS'], 'rustc': subprocess.check_output(['rustc', '-vV'], text=True),
                'sources': {p.name: sha(p) for p in source.iterdir()}, 'runs': []}
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    target = ROOT / '.local/snapshot-sequence-target'
    for label, extra in [('clippy', ['--all-targets', '--all-features', '--', '-D', 'warnings']),
                         ('build', ['--release', '--bins', '--features', 'model'])]:
        with (OUT / (label + '.log')).open('w') as log:
            subprocess.run(['cargo', label, '--locked', '--manifest-path', str(source / 'Cargo.toml'),
                            '--target-dir', str(target), *extra], stdout=log, stderr=subprocess.STDOUT, check=True)
    metadata['binaries'] = {}
    for name in ['snapshot-sequence-probe', 'snapshot-sequence-model']:
        shutil.copy2(target / 'release' / name, OUT / name)
        metadata['binaries'][name] = sha(OUT / name)
    with (OUT / 'models.jsonl').open('w') as output, (OUT / 'models.log').open('w') as log:
        result = subprocess.run([str(OUT / 'snapshot-sequence-model')], stdout=output, stderr=log)
    metadata['model_exit'] = result.returncode
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    assert result.returncode == 0, 'model failed; preserve all logs and counterexamples'
    with (OUT / 'controls.tsv').open('w') as log:
        subprocess.run([str(OUT / 'snapshot-sequence-probe'), 'verify'], stdout=log, check=True)
    cpu = min(os.sched_getaffinity(0))
    metadata['cpu'] = cpu
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
                subprocess.run(['taskset', '-c', str(cpu), str(OUT / 'snapshot-sequence-probe'), 'timing', variant], stdout=log, check=True)
            after = snapshot(cpu)
            delta = [b - a for a, b in zip(before, after)]
            assert all(v >= 0 for v in delta[:8]) and sum(delta[:8]) > 0
            metadata['runs'].append(dict(round=repeat, variant=variant, file=file,
                cpu_before=before, cpu_after=after, host_steal_fraction=delta[7] / sum(delta[:8])))
            (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    verify()


def verify():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    for name, expected in metadata['sources'].items():
        assert sha(OUT / 'source' / name) == expected
    for name, expected in metadata['binaries'].items():
        assert sha(OUT / name) == expected
    assert metadata['model_exit'] == 0
    models = [json.loads(line) for line in (OUT / 'models.jsonl').read_text().splitlines()]
    assert models[:2] == [{'case': 'timestamp-ABA', 'expected_counterexample': True},
                          {'case': 'version-without-fences', 'expected_counterexample': True}]
    assert len(models) == 4
    for row, writers, bound in zip(models[2:], [1, 2], [None, 2]):
        assert row['case'] == 'versioned' and row['writers'] == writers and row['preemption_bound'] == bound and row['schedules'] > 0
    assert (OUT / 'controls.tsv').read_text().splitlines() == [
        'timestamp_same_millisecond_accepts_mixed\ttrue',
        'version_same_millisecond_rejects_mixed\ttrue',
        'interrupted_writer_recovers_without_partial_commit\ttrue']
    assert [(r['round'], r['variant']) for r in metadata['runs']] == [(i, v) for i in range(8)
        for v in (['baseline', 'candidate'] if i % 2 == 0 else ['candidate', 'baseline'])]
    values = defaultdict(dict)
    for run in metadata['runs']:
        with (OUT / run['file']).open() as file:
            rows = list(csv.DictReader(file, delimiter='\t'))
        assert len(rows) == 80 and {r['case'] for r in rows} == set(CASES)
        for case in CASES:
            selected = [r for r in rows if r['case'] == case]
            assert [int(r['block']) for r in selected] == list(range(20))
            fields = 2 if case == 'read-2' else 20
            assert all(int(r['iterations']) == 1000000 and int(r['nanoseconds']) > 0
                       and int(r['checksum']) == 1000000 * fields for r in selected)
            values[case][run['round'], run['variant']] = sum(int(r['nanoseconds']) for r in selected) / 20000000
    summary = {'source': metadata['source'], 'scope': metadata['scope'], 'models': models,
               'controls': 3, 'raw_blocks': 1280, 'max_host_steal_fraction': max(r['host_steal_fraction'] for r in metadata['runs']), 'cases': {}}
    for case in CASES:
        changes = [values[case][i, 'candidate'] / values[case][i, 'baseline'] - 1 for i in range(8)]
        summary['cases'][case] = dict(baseline_ns=statistics.median(values[case][i, 'baseline'] for i in range(8)),
            candidate_ns=statistics.median(values[case][i, 'candidate'] for i in range(8)), paired_changes=changes, paired_median=statistics.median(changes))
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['run', 'verify'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        OUT = args.directory.resolve()
    globals()[args.mode]()
