#!/usr/bin/env python3
"""Verify downloaded full-node evidence without executing the measured binaries."""
import argparse
import hashlib
import itertools
import json
from pathlib import Path
import shutil
import zipfile


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def verify(root, commit, output):
    artifact = json.loads((root / 'artifact.json').read_text())
    manifest = json.loads((root / 'manifest.json').read_text())
    raw = (root / 'raw.zip').read_bytes()
    assert len(raw) == artifact['size_in_bytes'] == manifest['archive_bytes']
    assert 'sha256:' + digest(raw) == artifact['digest']
    assert digest(raw) == manifest['archive_sha256']
    assert artifact['workflow_run']['head_sha'] == commit
    with zipfile.ZipFile(root / 'raw.zip') as package:
        files = {row['path']: row for row in manifest['files']}
        assert {row.filename for row in package.infolist() if not row.is_dir()} == files.keys()
        for name, entry in files.items():
            content = package.read(name)
            assert len(content) == entry['bytes'] and digest(content) == entry['sha256']
        metadata = json.loads(package.read('metadata.json'))
        summary = json.loads(package.read('summary.json'))
        assert metadata['commit'] == commit
        assert summary['passed'] and summary['binaries_unchanged']
        assert summary['binaries'] == metadata['binaries']
        cases = []
        for name in sorted(files):
            if not name.endswith('.json'):
                continue
            row = json.loads(package.read(name))
            if 'exit_code' not in row:
                continue
            assert row['exit_code'] == 0 and row['passed'] and row['network_cleanup_ok']
            assert not row['remaining_namespaces']
            assert [link['ifname'] for link in row['remaining_links']] == ['lo']
            assert b'xdp-smoke: smoke completed' in package.read(name[:-5] + '.log')
            cases.append(row)
    labels = {f'{payload}-{idle}-{stats}-{n}-{wake}'
              for payload, idle, stats, n, wake in itertools.product(
                  ['copied', 'owned'], ['eager', 'cooperative'], ['bridge', 'full'], [1, 2], ['readable', 'worker'])}
    assert len(cases) == 32 and {row['label'] for row in cases} == labels
    tests = json.loads((root / 'test-summary.json').read_text())
    log = (root / 'job.log').read_bytes()
    assert tests['run'] == artifact['workflow_run']['id']
    assert digest(log) == tests['log_sha256']
    assert commit.encode() in log
    lines = [line.split('\t')[-1] for line in log.decode().splitlines() if 'test result:' in line]
    assert lines and lines == tests['test_results'] and all('test result: ok.' in line for line in lines)
    if output:
        output.mkdir(parents=True, exist_ok=True)
        for name in ['artifact.json', 'manifest.json', 'test-summary.json']:
            shutil.copy2(root / name, output / name)
        (output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
        (output / 'verified-node-cases.json').write_text(json.dumps(cases, indent=2) + '\n')
    print(json.dumps({'source': commit, 'files': len(files), 'native_combinations': len(cases),
                      'test_summaries': len(lines), 'result': 'PASS',
                      'scope': 'native correctness and provenance; no throughput or downloaded-node executable verification'}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('download', type=Path, help='raw.zip, artifact/manifest JSON and retained test job log')
    parser.add_argument('--commit', required=True)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    verify(args.download, args.commit, args.output)
