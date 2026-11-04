#!/usr/bin/env python3
"""Validate the production bitmap, its range-union fault control and restoration."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    source = Path('src/config/h3_ports.rs')
    pristine = source.read_bytes()
    command = ['cargo', 'test', '--locked', '--manifest-path', 'scripts/perf/h3-index/Cargo.toml']
    runs = []

    def run(label, fault=False):
        path = output/(label+'.log')
        with path.open('w') as log:
            code = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT).returncode
        text = path.read_text()
        assert 'could not compile' not in text, f'{label}: compilation failed'
        if fault:
            assert code != 0 and 'every_port_matches_range_union_at_word_boundaries ... FAILED' in text
            assert '1 passed; 1 failed' in text
        else:
            assert code == 0 and '2 passed; 0 failed' in text, label
        runs.append({'label': label, 'exit_code': code,
                     'source_sha256': hashlib.sha256(source.read_bytes()).hexdigest()})

    run('production')
    try:
        text = pristine.decode()
        token = '.fill(u64::MAX);'
        assert text.count(token) == 1
        source.write_text(text.replace(token, '.fill(0);'))
        run('missing-middle-words-mutant', fault=True)
    finally:
        source.write_bytes(pristine)
    assert source.read_bytes() == pristine
    run('restored-production')
    with (output/'clippy.log').open('w') as log:
        code = subprocess.run(['cargo', 'clippy', '--locked', '--manifest-path',
            'scripts/perf/h3-index/Cargo.toml', '--all-targets', '--',
            '-D', 'clippy::correctness', '-D', 'clippy::suspicious'],
            stdout=log, stderr=subprocess.STDOUT).returncode
    assert code == 0, 'production clippy failed'
    result = {'passed': True, 'source_restored': True, 'runs': runs,
              'source_sha256': hashlib.sha256(pristine).hexdigest(), 'clippy_exit_code': code,
              'scope': 'production bitmap full-u16 reference union and immutable clone; full parser/config epoch tests remain root gates'}
    (output/'summary.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
