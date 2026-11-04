#!/usr/bin/env python3
"""Verify native affinity controls, original sources and actual executable."""
import argparse
import csv
import hashlib
import io
import json
from pathlib import Path
import zipfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('archive', type=Path)
args = parser.parse_args()
evidence = Path(__file__).resolve().parent
manifest = json.loads((evidence / 'manifest.json').read_text())
assert args.archive.stat().st_size == manifest['archive_bytes']
assert hashlib.sha256(args.archive.read_bytes()).hexdigest() == manifest['archive_sha256']
with zipfile.ZipFile(args.archive) as archive:
    expected = {r['path']: r for r in manifest['files']}
    assert {r.filename for r in archive.infolist() if not r.is_dir()} == expected.keys()
    for name, row in expected.items():
        raw = archive.read(name)
        assert len(raw) == row['bytes'] and hashlib.sha256(raw).hexdigest() == row['sha256']
    metadata = json.loads(archive.read('metadata.json'))
    assert metadata == json.loads((evidence / 'metadata.json').read_text())
    assert hashlib.sha256(archive.read('affinity-probe')).hexdigest() == metadata['binary_sha256']
    for name, sha in metadata['sources'].items():
        assert hashlib.sha256(archive.read('source/' + name)).hexdigest() == sha
    assert b'2 passed; 0 failed' in archive.read('test.log')
    assert metadata['original_affinity'] == [0, 1, 2, 3]
    assert metadata['restricted_affinity'] == [1, 3]
    for name, result in metadata['results'].items():
        if name == 'baseline-assert':
            assert result['exit'] == result['expected_exit'] == 101
            assert b'automatic queue CPU must belong to the inherited affinity' in archive.read(name + '.log')
        else:
            assert result['exit'] == result['expected_exit'] == 0
            rows = list(csv.DictReader(io.StringIO(archive.read(name + '.log').decode()), delimiter='\t'))
            assert rows == [result['observation']]
            assert rows[0]['before'] == '1,3' and rows[0]['pinned'] == 'true'
            assert rows[0]['selected'] == rows[0]['after'] == ('0' if name.startswith('baseline') else '1')
print(json.dumps({'result': 'PASS', 'files': len(expected), 'source': metadata['source'], 'controls': 4}))
