#!/usr/bin/env python3
"""Reproduce either immutable stored-header component experiment."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('stage', choices=['candidate', 'integrated'])
parser.add_argument('download', type=Path)
args = parser.parse_args()
source = {'candidate': '13b375c3c556a09028a7b84f2f55cc4d6ac45d26',
          'integrated': 'ba474745fae3c08332d9e0862d9d5650550664ff'}[args.stage]
root = verify_archive(args.download, source)
spec = importlib.util.spec_from_file_location('frozen_headers', HERE / args.stage / 'frozen_probe.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.OUT = root
module.verify()
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((HERE / args.stage / 'verified-summary.json').read_text())
