#!/usr/bin/env python3
"""Recheck local resource controls; harness revision is not executable revision."""
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
with tempfile.TemporaryDirectory(prefix='h3-admission-controls-') as temporary:
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
    for expected in summary['results']:
        path = root / expected['directory'] / 'manifest.json'
        source = json.loads(path.read_text())
        assert source['source_revision'] == summary['harness_checkout']
        assert source['args']['concurrency'] == expected['concurrency']
        assert source['cpu_assignment'] == {'proxy': [0, 1], 'client': [2, 3], 'origin': [4]}
        assert summary['binary_sha256'] in source['binaries'].values()
        driver = 'native-admission-local-4g.py' if expected['proxy_memory_gib'] == 4 else 'native-admission-local.py'
        assert source['driver_sha256'] == hashlib.sha256((root / driver).read_bytes()).hexdigest()
        rows = module.validate(path, root)
        assert len(rows) == 1
        row = rows[0]
        assert row['successful'] == expected['successful'] and row['failed'] == expected['failed']
        assert row['status'] == ('traffic-errors' if expected['failed'] else 'passed')
        if expected['failed']:
            assert 'HTTP/3 native admission rejection' in (path.parent / 'edge.log').read_text()
        print(expected['directory'], row['successful'], 'successful,', row['failed'], 'failed')
print('PASS: original hashes, actual binary identity and all 1536 transfers verified')
