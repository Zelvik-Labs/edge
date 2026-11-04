#!/usr/bin/env python3
"""Check retained artifact bytes, then independently recompute every sample."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import statistics
import sys
import tempfile
import zipfile

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]


def verify(root, manifest):
    for entry in manifest['files']:
        file = root / entry['path']
        assert file.resolve().is_relative_to(root.resolve())
        assert file.stat().st_size == entry['bytes'], file
        assert hashlib.file_digest(file.open('rb'), 'sha256').hexdigest() == entry['sha256'], file
    subprocess.run([sys.executable, str(REPO / 'scripts/perf/http-state/verify.py'), str(root)], check=True)
    subprocess.run([sys.executable, str(REPO / 'scripts/perf/protocol_process_rss.py'), str(root)], check=True)
    assert json.loads((root / 'verified-records.json').read_text()) == json.loads((HERE / 'verified-records.json').read_text())
    for name in ['verified-cases.csv', 'verified-comparison.md', 'verified-rx.md', 'verified-process-rss.json', 'verified-process-rss.csv', 'verified-process-rss.md']:
        assert (root / name).read_bytes() == (HERE / name).read_bytes(), name
    rows = json.loads((root / 'verified-records.json').read_text())
    for expected in json.loads((HERE / 'verified-rx-pairs.json').read_text()):
        rps, cpu = [], []
        for repeat in range(3):
            values = {row['batch']: row for row in rows if row['manifest'].startswith(f'rx-round-{repeat}-') and row['cache'] == expected['cache'] and row['connection'] == expected['connection']}
            assert len(values) == 2 and all(row['status'] == 'passed' for row in values.values())
            rps.append(100 * (values['32']['rps'] / values['1']['rps'] - 1))
            cpu.append(100 * (values['32']['cpu_usec_per_request'] / values['1']['cpu_usec_per_request'] - 1))
        assert rps == expected['paired_rps_delta_pct'] and cpu == expected['paired_cpu_delta_pct']
        assert statistics.median(rps) == expected['median_rps_delta_pct']
        assert statistics.median(cpu) == expected['median_cpu_delta_pct']
    print('PASS: artifact files, all retained comparisons, RSS and RX paired arithmetic match')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--archive', type=Path)
    group.add_argument('--extracted', type=Path)
    args = parser.parse_args()
    manifest = json.loads((HERE / 'manifest.json').read_text())
    if args.extracted:
        verify(args.extracted, manifest)
        return
    assert args.archive.stat().st_size == manifest['archive_bytes']
    assert hashlib.file_digest(args.archive.open('rb'), 'sha256').hexdigest() == manifest['archive_sha256']
    with tempfile.TemporaryDirectory(prefix='edge-http-state-') as directory:
        root = Path(directory)
        with zipfile.ZipFile(args.archive) as archive:
            for entry in archive.infolist():
                assert (root / entry.filename).resolve().is_relative_to(root)
            archive.extractall(root)
        verify(root, manifest)


if __name__ == '__main__':
    main()
