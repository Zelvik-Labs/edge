#!/usr/bin/env python3
"""Verify the sustained failure without treating host contention as a code diagnosis."""
import hashlib
import importlib.util
import json
from pathlib import Path
import re
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
with tempfile.TemporaryDirectory(prefix='native-churn-failure-') as temporary:
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
    case = root / 'capacity-continuous-2g'
    source = json.loads((case / 'manifest.json').read_text())
    assert source['source_revision'] == summary['harness_checkout']
    assert source['binaries'] == summary['binary_digests']
    assert '3a17eeb3f5f5ddf61fa0084edf161ac7d326c4b45fa24f2ffb260b99290219c3' in source['binaries'].values()
    assert source['args']['concurrency'] == 32 and source['args']['fresh_requests'] == 262144
    assert source['cpu_assignment'] == {'proxy': [0, 1], 'client': [2, 3], 'origin': [4]}
    for name, key in [('protocol_state_matrix.py', 'driver_sha256'), ('protocol-state-client', 'client_sha256')]:
        assert hashlib.file_digest((root / name).open('rb'), 'sha256').hexdigest() == summary[key] == source[key]
    rows = module.validate(case / 'manifest.json', root)
    assert rows == summary['rows'] and len(rows) == 1
    assert rows[0]['successful'] == 260579 and rows[0]['failed'] == 1565 and rows[0]['status'] == 'traffic-errors'
    assert rows[0]['max_host_steal_fraction'] > 0.43
    lines = [re.sub(r'\x1b\[[0-9;]*m', '', line) for line in (case / 'edge.log').read_text().splitlines() if 'first admission rejection' in line]
    assert lines == summary['first_rejections'] and len(lines) == 1
    assert 'class="quiche_connection" workspace_used=0' in lines[0]
    samples = [json.loads(line) for line in (root / 'capacity-continuous-samples.jsonl').read_text().splitlines()]
    assert len(samples) == summary['sample_count'] == 68
    assert sum('fd_observation_error' in sample for sample in samples) == summary['fd_observation_errors'] == 68
    rss = [sample['node_rss_kib'] / 1024 for sample in samples]
    assert {'first': rss[0], 'last': rss[-1], 'min': min(rss), 'max': max(rss)} == summary['rss_mib']
print('PASS: artifact evidence and all 262144 transfers verified; measured traffic remains FAILED')
