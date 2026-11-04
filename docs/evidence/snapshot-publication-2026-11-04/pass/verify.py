#!/usr/bin/env python3
"""Recompute the exact archived diagnostic, including actual binary identity."""
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
root = verify_archive(args.download, '6fcb3dd82fc6925d25955951cddf4bb132b4f0f7')
spec = importlib.util.spec_from_file_location('frozen_diagnostic', HERE / 'frozen_probe.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.OUT = root
module.verify()
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((HERE / 'verified-summary.json').read_text())
