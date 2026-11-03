#!/usr/bin/env python3
"""Recheck same-host resource controls, keeping runtime and harness sources distinct."""
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
with tempfile.TemporaryDirectory(prefix='h3-capacity-controls-') as temporary:
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
    driver = (root / 'protocol_state_matrix.py').read_bytes()
    client = (root / 'protocol-state-client').read_bytes()
    assert b"'--memory','2g','--memory-swap','2g'" in driver
    assert b"'RUST_LOG=warn'" in driver
    controllers = set()
    results = []
    for expected in summary['results']:
        path = root / expected['directory'] / 'manifest.json'
        source = json.loads(path.read_text())
        assert source['source_revision'] == expected['harness_checkout']
        assert source['binaries'] == expected['binary_digests']
        assert source['args']['concurrency'] == 32 and source['args']['repeats'] == 3
        assert source['cpu_assignment'] == {'proxy': [0, 1], 'client': [2, 3], 'origin': [4]}
        assert hashlib.sha256(driver).hexdigest() == source['driver_sha256'] == expected['driver_sha256']
        assert hashlib.sha256(client).hexdigest() == source['client_sha256'] == expected['client_sha256']
        controllers.add(next(digest for name, digest in source['binaries'].items() if name.endswith('issue34-controller')))
        rows = module.validate(path, root)
        assert rows == expected['rows'] and len(rows) == 6
        results.append(rows)
        print(expected['directory'], [(mode, sum(r['failed'] for r in rows if r['connection'] == mode)) for mode in ['fresh', 'reuse']])
    assert len(controllers) == 1
    assert [sum(row['failed'] for row in rows if row['connection'] == 'fresh') for rows in results] == [1144, 114, 0]
    assert [sum(row['failed'] for row in rows if row['connection'] == 'reuse') for rows in results] == [3603, 0, 0]
    assert all(row['status'] == 'passed' for row in results[-1])
    assert sum(row['successful'] + row['failed'] for rows in results for row in rows) == 170500
print('PASS: all hashes and 170500 transfers; fixed driver/client/controller, 2 GiB and 32 concurrency')
