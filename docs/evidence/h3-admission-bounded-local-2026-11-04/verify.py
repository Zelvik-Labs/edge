#!/usr/bin/env python3
"""Recheck the bounded-ingress 4 GiB control independently of CI ranking."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import zipfile

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
spec = importlib.util.spec_from_file_location('protocol_verify', REPO / 'scripts/perf/http-state/verify.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
manifest = json.loads((HERE / 'manifest.json').read_text())
summary = json.loads((HERE / 'summary.json').read_text())
raw = HERE / 'raw.zip'
assert raw.stat().st_size == manifest['archive_bytes']
assert hashlib.file_digest(raw.open('rb'), 'sha256').hexdigest() == manifest['archive_sha256']
with tempfile.TemporaryDirectory(prefix='bounded-h3-control-') as temporary:
    root = Path(temporary)
    with zipfile.ZipFile(raw) as archive:
        for entry in manifest['files']:
            path = root / entry['path']
            assert path.resolve().is_relative_to(root)
            body = archive.read(entry['path'])
            assert len(body) == entry['bytes'] and hashlib.sha256(body).hexdigest() == entry['sha256']
            assert b'PRIVATE KEY-----' not in body
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(body)
    path = root / 'admission-local-bounded-4g/manifest.json'
    source = json.loads(path.read_text())
    assert source['source_revision'] == summary['harness_checkout']
    assert source['binaries'] == summary['binary_digests']
    assert source['args']['concurrency'] == 32
    assert source['cpu_assignment'] == {'proxy': [0, 1], 'client': [2, 3], 'origin': [4]}
    driver = (root / 'native-admission-bounded-4g.py').read_bytes()
    assert hashlib.sha256(driver).hexdigest() == summary['driver_sha256'] == source['driver_sha256']
    assert b"'--memory','4g','--memory-swap','4g'" in driver
    assert b"'RUST_LOG=warn'" in driver
    rows = module.validate(path, root)
    assert rows == summary['rows']
    assert len(rows) == 3 and all(row['successful'] == 512 and row['failed'] == 0 and row['status'] == 'passed' for row in rows)
print('PASS: original hashes, harness/binary identities and all 1536 transfers verified')
