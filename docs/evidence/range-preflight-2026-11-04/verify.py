#!/usr/bin/env python3
"""Verify both archives, actual binaries and frozen Range comparison logic."""
import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import sys
import tempfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('evidence', type=Path)
parser.add_argument('binaries', type=Path)
args = parser.parse_args()
root = verify_archive(args.evidence, 'd8d67ffa2cdd7802552d6c2a2a259fbe1be215ad')
binaries = verify_archive(args.binaries, 'd8d67ffa2cdd7802552d6c2a2a259fbe1be215ad')
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    helper = workspace / 'scripts/perf/http-state/verify.py'
    helper.parent.mkdir(parents=True)
    shutil.copy2(HERE / 'http_verify.py', helper)
    spec = importlib.util.spec_from_file_location('frozen_range', HERE / 'frozen_verify.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = workspace
    module.verify(root, binaries / 'cache-range-binaries')
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((HERE / 'verified-summary.json').read_text())
