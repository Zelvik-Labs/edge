#!/usr/bin/env python3
"""Repeat one real H3 regression; failed attempts are never replaced by retries."""
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path.cwd()
OUT = ROOT / '.local/h3-drain-repeat-ci'
TEST = 'origin_h3::wire_tests::failed_stream_drains_new_reuse_without_cancelling_an_active_peer'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    OUT.mkdir(parents=True, exist_ok=False)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'scope': '64 actual kernel-loopback H3 drain/replacement regressions; correctness only, not throughput or TLS certificate verification',
        'test': TEST, 'rustflags': os.environ['RUSTFLAGS'],
        'sources': {p: sha(ROOT / p) for p in ['src/origin_h3_tests.rs', 'scripts/perf/h3-drain-repeat/run.py']},
        'runs': []}
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    with (OUT / 'build.jsonl').open('w') as output, (OUT / 'build.log').open('w') as log:
        subprocess.run(['cargo', 'test', '--locked', '--lib', '--no-run', '--message-format=json'],
                       stdout=output, stderr=log, check=True)
    builds = [json.loads(line) for line in (OUT / 'build.jsonl').read_text().splitlines()]
    paths = [r['executable'] for r in builds if r.get('reason') == 'compiler-artifact'
             and r['target']['name'] == 'zelvik_edge' and r.get('executable') and r['profile']['test']]
    assert len(paths) == 1
    binary = Path(paths[0])
    metadata['test_binary'] = {'path': str(binary), 'bytes': binary.stat().st_size, 'sha256': sha(binary),
        'archived': False}
    for number in range(64):
        directory = OUT / f'case-{number:02d}'
        directory.mkdir()
        env = {**os.environ, 'EDGE_H3_TEST_OUTPUT': str(directory)}
        with (directory / 'test.log').open('w') as log:
            result = subprocess.run([str(binary), TEST, '--exact', '--nocapture'],
                                    env=env, stdout=log, stderr=subprocess.STDOUT, timeout=30)
        row = {'case': number, 'exit': result.returncode, 'log_sha256': sha(directory / 'test.log')}
        metadata['runs'].append(row)
        (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
        assert result.returncode == 0, f'actual H3 repeat {number} failed; retain this attempt'
        reports = list(directory.glob('*.json'))
        assert len(reports) == 1
        report = json.loads(reports[0].read_text())
        assert report['passed'] and report['test'] == TEST
        assert report['accepted_quic_connections'] == report['closed_quic_connections'] == 2
        assert report['resolved_h3_requests'] == 3
        assert 'test result: ok. 1 passed; 0 failed' in (directory / 'test.log').read_text()
    assert sha(binary) == metadata['test_binary']['sha256']
    summary = {'source': metadata['source'], 'scope': metadata['scope'], 'completed': 64,
               'failures': 0, 'accepted_and_closed_connections': 128, 'resolved_h3_requests': 192,
               'binary_unchanged': True, 'actual_test_elf_in_artifact': False}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary))


if __name__ == '__main__':
    main()
