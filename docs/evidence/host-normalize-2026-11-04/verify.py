#!/usr/bin/env python3
"""Recompute the original host string comparison from every archived byte."""
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
root = verify_archive(args.download, '093bb23b211b9e96685cd8f9e82a9b9e39d4c9e2')
spec = importlib.util.spec_from_file_location('frozen_host', HERE / 'frozen_probe.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.OUT = root
module.verify()
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((HERE / 'verified-summary.json').read_text())
