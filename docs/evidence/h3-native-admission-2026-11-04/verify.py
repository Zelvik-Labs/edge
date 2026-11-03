#!/usr/bin/env python3
"""Verify the final admission diagnosis against all original artifact bytes."""
import argparse
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


def verify(root, manifest, binaries):
    for entry in manifest['files']:
        path = root / entry['path']
        assert path.resolve().is_relative_to(root.resolve())
        assert path.stat().st_size == entry['bytes']
        assert hashlib.file_digest(path.open('rb'), 'sha256').hexdigest() == entry['sha256']
    path = root / 'admission-quiche/manifest.json'
    source = json.loads(path.read_text())
    assert source['source_revision'] == '74f53256ba85ad0ed3bae8424db7d21a037c10c2'
    assert source['args']['edge_backend'] == 'tokio-quiche-native'
    assert source['args']['transport_profile'] == 'matched' and source['args']['concurrency'] == 32
    assert hashlib.file_digest((root / 'protocol-state-client').open('rb'), 'sha256').hexdigest() == source['client_sha256']
    if binaries:
        for name, digest in source['binaries'].items():
            matches = list(binaries.rglob(Path(name).name))
            assert len(matches) == 1
            assert hashlib.file_digest(matches[0].open('rb'), 'sha256').hexdigest() == digest
    rows = module.validate(path, root)
    logs = []
    for file in [*sorted(path.parent.glob('edge-boot-*.log')), path.parent / 'edge.log']:
        for line in file.read_text().splitlines():
            if 'first admission rejection' in line:
                logs.append({'file': file.name, 'line': re.sub(r'\x1b\[[0-9;]*m', '', line)})
    assert len(rows) == len(logs) == 3
    assert [(row['successful'], row['failed']) for row in rows] == [(62, 450), (57, 455), (61, 451)]
    for entry in logs:
        assert 'class="quiche_transport_bytes" workspace_used=498073600' in entry['line']
        assert 'rejects_total: 0' in entry['line']
    assert {'rows': rows, 'first_rejections': logs} == json.loads((HERE / 'verified-admission.json').read_text())
    print('PASS: 3 rounds, 1536 transfers and exact workspace rejection; shared-account rejects remain zero')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--archive', type=Path)
    group.add_argument('--extracted', type=Path)
    parser.add_argument('--binaries', type=Path)
    args = parser.parse_args()
    manifest = json.loads((HERE / 'manifest.json').read_text())
    if args.extracted:
        verify(args.extracted, manifest, args.binaries)
        return
    assert args.archive.stat().st_size == manifest['archive_bytes']
    assert hashlib.file_digest(args.archive.open('rb'), 'sha256').hexdigest() == manifest['archive_sha256']
    with tempfile.TemporaryDirectory(prefix='h3-admission-ci-') as temporary:
        root = Path(temporary)
        with zipfile.ZipFile(args.archive) as archive:
            for entry in archive.infolist():
                assert (root / entry.filename).resolve().is_relative_to(root)
            archive.extractall(root)
        verify(root, manifest, args.binaries)


if __name__ == '__main__':
    main()
