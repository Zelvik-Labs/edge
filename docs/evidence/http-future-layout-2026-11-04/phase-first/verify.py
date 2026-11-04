#!/usr/bin/env python3
"""Retain the failed equality assertion without relabelling its workflow."""
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
expected = json.loads((HERE / 'observations.json').read_text())
root = verify_archive(args.download, expected['source'])
data = root / '.local/http-future-layout-ci'
for name in ['layout.json', 'phases.json']:
    assert json.loads((data / name).read_text()) == json.loads((HERE / name).read_text())
assert hashlib.sha256((root / 'target/release/examples/http-future-layout').read_bytes()).hexdigest() == expected['binary_sha256']
raw = (data / 'phases-test.log').read_bytes()
assert hashlib.sha256(raw).hexdigest() == expected['unit_log_sha256']
assert b'test proxy::tests::request_phase_future_layout ... ok' in raw
assert b'test result: ok. 1 passed;' in raw
assert json.loads((data / 'layout.json').read_text())['callbacks']['request_filter']['frame_bytes'] == 5736
assert json.loads((data / 'phases.json').read_text())['request_filter_frame_bytes'] == 5808
assert not (data / 'metadata.json').exists()
print(json.dumps(expected, indent=2))
