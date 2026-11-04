#!/usr/bin/env python3
"""Prove sparse affinity selection and the old failure using native threads."""
import csv
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/affinity-ci'
BASE = '5740a0e26581d9d06b56c22278f1c9059823d05c'


def function(source, signature):
    start = source.index(signature)
    return source[start:source.index('\n}\n', start) + 3]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def mask_ids(value):
    result = []
    for item in value.split(','):
        bounds = [int(part) for part in item.split('-')]
        result.extend(range(bounds[0], bounds[-1] + 1))
    return result


def main():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    baseline = subprocess.check_output(['git', 'rev-parse', BASE], cwd=ROOT, text=True).strip()
    old = subprocess.check_output(['git', 'show', baseline + ':src/xdp/af_xdp/bridge.rs'], cwd=ROOT, text=True)
    new = (ROOT / 'src/xdp/af_xdp/bridge.rs').read_text()
    selection = function(old, 'pub(crate) fn af_xdp_queue_cpu(').replace('af_xdp_queue_cpu', 'baseline_queue_cpu')
    selection += '\n' + function(old, 'pub(crate) fn pin_current_thread_to_cpu(').replace('pin_current_thread_to_cpu', 'baseline_pin')
    selection += '\n' + function(new, 'fn af_xdp_queue_cpu(').replace('af_xdp_queue_cpu', 'candidate_queue_cpu')
    (source / 'selection.rs').write_text(selection)
    (source / 'baseline-original.rs').write_text(old)
    (source / 'candidate-original.rs').write_text(new)
    shutil.copy2(ROOT / 'src/xdp/af_xdp/affinity.rs', source / 'affinity.rs')
    shutil.copy2(ROOT / 'benches/support/affinity_probe.rs', source / 'main.rs')
    manifest = (ROOT / 'scripts/perf/affinity-probe/Cargo.toml').read_text().replace(
        '../../../benches/support/affinity_probe.rs', 'main.rs')
    (source / 'Cargo.toml').write_text(manifest)
    shutil.copy2(ROOT / 'scripts/perf/affinity-probe/Cargo.lock', source / 'Cargo.lock')
    target = ROOT / '.local/affinity-target'
    for mode, extra in [('test', ['--', '--nocapture']), ('build', ['--release'])]:
        command = ['cargo', mode, '--locked', '--manifest-path', str(source / 'Cargo.toml'), '--target-dir', str(target), *extra]
        with (OUT / (mode + '.log')).open('w') as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
    binary = OUT / 'affinity-probe'
    shutil.copy2(target / 'release/affinity-probe', binary)
    original = sorted(os.sched_getaffinity(0))
    assert len(original) >= 2
    restricted = sorted(set([original[1], original[-1]]))
    assert 0 not in restricted
    env = dict(os.environ, EDGE_AFFINITY_EXPECTED=','.join(map(str, restricted)))
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                'baseline': baseline, 'original_affinity': original, 'restricted_affinity': restricted,
                'binary_sha256': sha(binary), 'rustc': subprocess.check_output(['rustc', '-vV'], text=True),
                'sources': {path.name: sha(path) for path in source.iterdir() if path.is_file()},
                'scope': 'queue-zero CPU placement and explicit selection compatibility; no throughput claim', 'results': {}}
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    for variant in ['baseline', 'candidate']:
        for control in [False, True]:
            name = variant + ('-assert' if control else '-observe')
            command = ['taskset', '-c', env['EDGE_AFFINITY_EXPECTED'], str(binary), variant]
            if control:
                command.append('--assert-budget')
            with (OUT / (name + '.log')).open('w') as log:
                result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
            expected = 101 if variant == 'baseline' and control else 0
            metadata['results'][name] = {'exit': result.returncode, 'expected_exit': expected}
            (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
            assert result.returncode == expected
            if expected:
                assert 'automatic queue CPU must belong to the inherited affinity' in (OUT / (name + '.log')).read_text()
            else:
                with (OUT / (name + '.log')).open() as log:
                    rows = list(csv.DictReader(log, delimiter='\t'))
                assert len(rows) == 1 and mask_ids(rows[0]['before']) == restricted
                metadata['results'][name]['observation'] = rows[0]
                if variant == 'candidate':
                    assert int(rows[0]['selected']) == restricted[0]
                    assert mask_ids(rows[0]['after']) == [restricted[0]] and rows[0]['pinned'] == 'true'
    assert sha(binary) == metadata['binary_sha256']
    assert sorted(os.sched_getaffinity(0)) == original
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(json.dumps(metadata, indent=2))


if __name__ == '__main__':
    main()
