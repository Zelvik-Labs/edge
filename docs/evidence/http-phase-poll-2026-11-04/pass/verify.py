#!/usr/bin/env python3
"""Verify original production-method probe bytes and recompute every block."""
import argparse
import importlib.util
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
root = verify_archive(args.download, '61e525b85d021acb518af09f1f170132b796f317')
assert json.loads((root / 'metadata.json').read_text()) == json.loads((HERE / 'metadata.json').read_text())
spec = importlib.util.spec_from_file_location('frozen_actual_phase', HERE / 'frozen_probe.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.OUT = root
module.verify()
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((HERE / 'verified-summary.json').read_text())
