#!/usr/bin/env python3
"""Verify the failed first run and its retained actual executable archive."""
import argparse
from collections import Counter
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import zipfile

HERE = Path(__file__).resolve().parent


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def archive(directory, prefix):
    artifact = json.loads((HERE / (prefix + 'artifact.json')).read_text())
    manifest = json.loads((HERE / (prefix + 'manifest.json')).read_text())
    raw = (directory / 'raw.zip').read_bytes()
    assert len(raw) == artifact['size_in_bytes'] == manifest['archive_bytes']
    assert 'sha256:' + sha(raw) == artifact['digest']
    assert sha(raw) == manifest['archive_sha256']
    with zipfile.ZipFile(directory / 'raw.zip') as package:
        files = {row['path']: row for row in manifest['files']}
        assert {row.filename for row in package.infolist() if not row.is_dir()} == files.keys()
        for name, expected in files.items():
            data = package.read(name)
            assert len(data) == expected['bytes'] and sha(data) == expected['sha256']
            assert (directory / 'extracted' / name).read_bytes() == data
    return directory / 'extracted'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('evidence', type=Path)
    parser.add_argument('binaries', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    root = archive(args.evidence, '')
    binaries = archive(args.binaries, 'binary-') / 'cache-range-binaries'
    build = json.loads((binaries / 'build-metadata.json').read_text())
    assert build == json.loads((root / 'build-metadata.json').read_text())
    for variant, entry in build['variants'].items():
        for name, expected in entry['binaries'].items():
            data = (binaries / variant / name).read_bytes()
            assert len(data) == expected['bytes'] and sha(data) == expected['sha256']
    spec = importlib.util.spec_from_file_location('frozen_http', HERE / 'http_verify.py')
    http = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(http)
    rows = http.validate(root / 'round-0-before/manifest.json', root)
    assert len(rows) == 12 and all(row['status'] == 'passed' for row in rows)
    failed = json.loads((root / 'round-0-after/manifest.json').read_text())
    assert failed['status'] == 'failed' and failed['cases'] == []
    assert failed['error'] == 'direct origin capacity reference has transfer errors'
    assert not failed.get('cleanup_errors')
    references = []
    for shard, expected_errors in [(0, 85), (1, 83)]:
        file = root / 'round-0-after' / f'origin-reference.{shard}.client.tsv.gz'
        with gzip.open(file, 'rt') as source:
            raw = [line.strip().split('\t') for line in source]
        errors = [i for i, row in enumerate(raw) if row[0] != '200' or row[6] != '0']
        assert errors == list(range(expected_errors))
        assert all(raw[i][0] == raw[i][1] == raw[i][3] == '0' for i in errors)
        distribution = dict(Counter(raw[i][6] for i in errors))
        assert distribution == ({'56': 83, '52': 2} if shard == 0 else {'56': 78, '52': 5})
        assert all(row[0] == '200' and row[1] == '1.1' and row[3] == '1024' and row[6] == '0'
                   for row in raw[expected_errors:])
        references.append({'shard': shard, 'transfers': len(raw), 'startup_failures': len(errors), 'curl_errors': distribution,
                           'error_indices': [errors[0], errors[-1]], 'later_transfers_all_valid': True})
    result = {'run': 41712678098, 'source': build['harness_source'],
              'before_completed_cells': len(rows), 'before_successful': sum(row['successful'] for row in rows),
              'candidate_cells': 0, 'failed_origin_reference': references,
              'actual_executables_verified': 4, 'scope': 'failed startup evidence; no before/after ranking'}
    if args.output:
        args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
