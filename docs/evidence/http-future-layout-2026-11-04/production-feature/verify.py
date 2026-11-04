#!/usr/bin/env python3
"""Verify the same-build production phase probe retained by full CI."""
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
for name in ['artifact.json', 'manifest.json']:
    assert json.loads((args.download / name).read_text()) == json.loads((HERE / name).read_text())
root = verify_archive(args.download, '11d4d6f7420d853f34161a472804179002d7d11e')
metadata = json.loads((root / '.local/http-future-layout-ci/metadata.json').read_text())
assert metadata == json.loads((HERE / 'metadata.json').read_text())
assert metadata['features'] == ['http-layout-probe']
assert hashlib.sha256((root / 'target/release/examples/http-future-layout').read_bytes()).hexdigest() == metadata['binary_sha256']
layout = json.loads((root / '.local/http-future-layout-ci/layout.json').read_text())
phases = json.loads((root / '.local/http-future-layout-ci/phases.json').read_text())
assert layout == json.loads((HERE / 'layout.json').read_text())
assert phases == json.loads((HERE / 'phases.json').read_text()) == layout['request_phases']
assert len(phases['phases']) == 11
assert phases['request_filter_frame_bytes'] == layout['callbacks']['request_filter']['frame_bytes'] == 5736
assert phases['phases']['body_waf']['frame_bytes'] == 5600
print('PASS all five artifact files, actual release probe and same-build phase layouts')
