#!/usr/bin/env python3
"""Verify every retained wire attempt without treating test-ELF metadata as its bytes."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1]))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('download', type=Path)
args = parser.parse_args()
source = '436e8fdd81919f2d35f292c2519c0383805f5fb3'
for name in ['artifact.json', 'manifest.json']:
    assert json.loads((args.download / name).read_text()) == json.loads((HERE / name).read_text())
root = verify_archive(args.download, source)
metadata = json.loads((root / 'metadata.json').read_text())
summary = json.loads((root / 'verified-summary.json').read_text())
for name, row in [('metadata.json', metadata), ('verified-summary.json', summary)]:
    assert row == json.loads((HERE / name).read_text())
assert metadata['source'] == summary['source'] == source
assert not metadata['test_binary']['archived'] and not summary['actual_test_elf_in_artifact']
for name, expected in metadata['sources'].items():
    assert hashlib.sha256((HERE / 'source' / name).read_bytes()).hexdigest() == expected
assert [r['case'] for r in metadata['runs']] == list(range(64))
accepted = closed = requests = 0
for row in metadata['runs']:
    directory = root / f"case-{row['case']:02d}"
    assert row['exit'] == 0
    log = (directory / 'test.log').read_bytes()
    assert hashlib.sha256(log).hexdigest() == row['log_sha256']
    assert b'test result: ok. 1 passed; 0 failed' in log
    reports = list(directory.glob('*.json'))
    assert len(reports) == 1
    report = json.loads(reports[0].read_text())
    assert report['passed'] and report['test'] == metadata['test']
    assert report['accepted_quic_connections'] == report['closed_quic_connections'] == 2
    assert report['resolved_h3_requests'] == 3
    accepted += report['accepted_quic_connections']
    closed += report['closed_quic_connections']
    requests += report['resolved_h3_requests']
assert summary['completed'] == 64 and summary['failures'] == 0
assert accepted == closed == summary['accepted_and_closed_connections'] == 128
assert requests == summary['resolved_h3_requests'] == 192 and summary['binary_unchanged']
print('PASS 64 retained H3 attempts, 128 accepted/closed connections, 192 requests; test ELF not archived')
