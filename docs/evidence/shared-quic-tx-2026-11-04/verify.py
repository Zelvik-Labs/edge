#!/usr/bin/env python3
"""Verify candidate source, executable identity and real Linux controls."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('download', type=Path)
args = parser.parse_args()
root = verify_archive(args.download, '27a47d67d54c95087af7c2f24ef0cc844a0e192e')
spec = importlib.util.spec_from_file_location('frozen_shared_tx', HERE / 'frozen_probe.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.OUT = root
module.verify()
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((HERE / 'verified-summary.json').read_text())
