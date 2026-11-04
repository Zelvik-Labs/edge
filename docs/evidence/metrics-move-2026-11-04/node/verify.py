#!/usr/bin/env python3
"""Verify the isolated metric ownership node comparison from both original downloads."""
import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import sys
import tempfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1]))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('evidence', type=Path)
parser.add_argument('binaries', type=Path)
args = parser.parse_args()
source = '0fc9cbd612c0d1cdefd5389ad903c946f99fbe0c'
original = verify_archive(args.evidence, source)
binaries = verify_archive(args.binaries, source)
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    helper = workspace / 'scripts/perf/http-state/verify.py'
    helper.parent.mkdir(parents=True)
    shutil.copy2(HERE / 'http_verify.py', helper)
    root = workspace / 'evidence'
    shutil.copytree(original, root)
    spec = importlib.util.spec_from_file_location('frozen_metrics_node', HERE / 'frozen_verify.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = workspace
    module.verify(root, binaries / 'cache-range-binaries')
    for name in ['verified-summary.json', 'verified-records.json']:
        assert json.loads((root / name).read_text()) == json.loads((HERE / name).read_text())
