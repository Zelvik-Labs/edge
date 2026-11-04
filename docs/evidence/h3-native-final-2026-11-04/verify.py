#!/usr/bin/env python3
"""Check retained artifact bytes, then independently recompute every sample."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]


def verify(root, manifest, binaries):
    for entry in manifest['files']:
        file = root / entry['path']
        assert file.resolve().is_relative_to(root.resolve())
        assert file.stat().st_size == entry['bytes'], file
        assert hashlib.file_digest(file.open('rb'), 'sha256').hexdigest() == entry['sha256'], file
    subprocess.run([sys.executable, str(REPO / 'scripts/perf/http-state/verify.py'), str(root), *(['--binaries', str(binaries)] if binaries else [])], check=True)
    subprocess.run([sys.executable, str(REPO / 'scripts/perf/protocol_process_rss.py'), str(root)], check=True)
    assert json.loads((root / 'verified-records.json').read_text()) == json.loads((HERE / 'verified-records.json').read_text())
    for name in ['verified-cases.csv', 'verified-native.md', 'verified-process-rss.json', 'verified-process-rss.csv', 'verified-process-rss.md']:
        assert (root / name).read_bytes() == (HERE / name).read_bytes(), name
    print('PASS: artifact files and all retained comparison results match')


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
    with tempfile.TemporaryDirectory(prefix='edge-http-state-') as directory:
        root = Path(directory)
        with zipfile.ZipFile(args.archive) as archive:
            for entry in archive.infolist():
                assert (root / entry.filename).resolve().is_relative_to(root)
            archive.extractall(root)
        verify(root, manifest, args.binaries)


if __name__ == '__main__':
    main()
