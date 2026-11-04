#!/usr/bin/env python3
"""Exercise the production helper and prove both fairness controls detect faults."""
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
    source = Path('src/xdp/af_xdp/cooperation.rs')
    pristine = source.read_bytes()
    sha = hashlib.sha256(pristine).hexdigest()
    command = ['cargo', 'test', '--locked', '--manifest-path', 'scripts/perf/idle-yield/Cargo.toml']
    results = []

    def run(label, failure=None):
        path = output/(label+'.log')
        with path.open('w') as log:
            code = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT).returncode
        text = path.read_text()
        assert 'could not compile' not in text, f'{label}: compilation failed'
        if failure is None:
            assert code == 0 and '2 passed; 0 failed' in text, label
        else:
            assert code != 0 and failure+' ... FAILED' in text, label
            assert '1 passed; 1 failed' in text, label
        results.append({'label': label, 'exit_code': code, 'expected_failing_test': failure,
                        'actual_source_sha256': hashlib.sha256(source.read_bytes()).hexdigest()})

    run('production')
    try:
        text = pristine.decode()
        token = 'tokio::task::consume_budget().await;'
        assert text.count(token) == 1
        source.write_text(text.replace(token, 'std::future::ready(()).await;'))
        run('no-idle-budget-mutant', 'ready_idle_rounds_allow_other_task_progress')
        text = pristine.decode()
        token = 'tokio::task::yield_now().await;'
        assert text.count(token) == 1
        source.write_text(text.replace(token, 'tokio::task::consume_budget().await;'))
        run('busy-budget-mutant', 'busy_rounds_remain_pending_on_the_first_poll')
    finally:
        source.write_bytes(pristine)
    assert hashlib.sha256(source.read_bytes()).hexdigest() == sha
    run('restored-production')
    with (output/'clippy.log').open('w') as log:
        code = subprocess.run(['cargo', 'clippy', '--locked', '--manifest-path',
            'scripts/perf/idle-yield/Cargo.toml', '--all-targets', '--',
            '-D', 'clippy::correctness', '-D', 'clippy::suspicious'],
            stdout=log, stderr=subprocess.STDOUT).returncode
    assert code == 0, 'production clippy failed'
    report = {'passed': True, 'source_sha256': sha, 'source_restored': True, 'runs': results,
              'clippy_exit_code': code,
              'scope': 'real current-thread ready-task progress and busy-round first-poll contract; no throughput or loaded network timer claim'}
    (output/'summary.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
