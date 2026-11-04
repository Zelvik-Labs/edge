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
root = verify_archive(args.download, '6ebb617dd91caf44670b990e55194bd141fb7fad')
log = (root / 'timing-clippy.log').read_text()
assert 'chunks_exact_to_as_chunks' in log and 'as_chunks::<32>()' in log
assert not (root / 'timing').exists() and not (root / 'verified-summary.json').exists()
assert not (root / 'controls.tsv').exists()
print('PASS original build failure retained; zero measured cells')
