#!/usr/bin/env python3
"""Verify a frozen IP lookup candidate with every differential result."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('stage', choices=['first'])
parser.add_argument('download', type=Path)
args = parser.parse_args()
stage = HERE / args.stage
source = 'dc7450a68739dead753097e80e79e2499bd0cda8'
root = verify_archive(args.download, source)
spec = importlib.util.spec_from_file_location('frozen_ip', stage / 'frozen_probe.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.OUT = root
module.verify()
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((stage / 'verified-summary.json').read_text())
