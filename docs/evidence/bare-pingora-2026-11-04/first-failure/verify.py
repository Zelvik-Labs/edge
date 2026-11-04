#!/usr/bin/env python3
"""Verify a premeasurement build failure; there is no node result here."""
import argparse
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
root = verify_archive(args.download, '953cf477b282c0093d149ba0de1ed76d19d3a8ad')
log = (root / 'clippy.log').read_text()
assert 'error[E0433]' in log and 'tokio' in log and 'File::from_std' in log
assert not (root / 'bare-pingora-proxy').exists() and not (root / 'verified-summary.json').exists()
assert not (root / 'records').exists()
print('PASS original build failure retained; zero measured cells')
