#!/usr/bin/env python3
"""Verify complete SNI comparison archives against their frozen calculations."""
import argparse
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import zipfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('phase', choices=['phase2', 'hybrid'])
parser.add_argument('archive', type=Path)
args = parser.parse_args()
evidence = Path(__file__).resolve().parent / args.phase
manifest = json.loads((evidence / 'manifest.json').read_text())
assert args.archive.stat().st_size == manifest['archive_bytes']
assert hashlib.sha256(args.archive.read_bytes()).hexdigest() == manifest['archive_sha256']
spec = importlib.util.spec_from_file_location('frozen_comparison', evidence / 'comparison.py')
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)
with tempfile.TemporaryDirectory(prefix='sni-cursor-evidence-') as temp:
    root = Path(temp)
    with zipfile.ZipFile(args.archive) as package:
        expected = {row['path']: row for row in manifest['files']}
        assert {row.filename for row in package.infolist() if not row.is_dir()} == expected.keys()
        for name, row in expected.items():
            target = root / name
            assert target.resolve().is_relative_to(root)
            raw = package.read(name)
            assert len(raw) == row['bytes']
            assert hashlib.sha256(raw).hexdigest() == row['sha256']
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(raw)
    metadata = json.loads((root / 'build-metadata.json').read_text())
    assert metadata == json.loads((evidence / 'build-metadata.json').read_text())
    for variant, data in metadata['variants'].items():
        for name, sha in data['binaries'].items():
            assert comparison.digest(root / variant / name) == sha
        for name in ['parser', 'probe']:
            assert comparison.digest(root / variant / (name + '.rs')) == data[name + '_sha256']
    tests = 8 if args.phase == 'phase2' else 9
    assert f'{tests} passed; 0 failed' in (root / 'tests.log').read_text()
    assert 'incomplete untrusted length must not allocate' in (root / 'baseline/assert-bounded.log').read_text()
    comparison.OUT = root
    with contextlib.redirect_stdout(io.StringIO()):
        comparison.verify()
    assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((evidence / 'verified-summary.json').read_text())
print(json.dumps({'phase': args.phase, 'files_verified': len(expected), 'source': metadata['source'], 'result': 'PASS'}))
