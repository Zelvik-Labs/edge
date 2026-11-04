#!/usr/bin/env python3
"""Recompute unchanged pacing failures, actual binaries and relay controls."""
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
source = '7df4a6e402f9421d2db030a1d79ba95dcbd5fc8d'
original = verify_archive(args.evidence, source)
binaries = verify_archive(args.binaries, source)
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    helper = workspace / 'scripts/perf/http-state/verify.py'
    helper.parent.mkdir(parents=True)
    shutil.copy2(HERE / 'http_verify.py', helper)
    root = workspace / 'evidence'
    shutil.copytree(original, root)
    spec = importlib.util.spec_from_file_location('frozen_pacing', HERE / 'frozen_verify.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = workspace
    module.verify(root, binaries / 'quinn-diagnostic-binaries')
    spec = importlib.util.spec_from_file_location('pacing_pattern', HERE / 'pattern.py')
    pattern = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(pattern)
    pattern.verify(root)
    for name in ['verified-summary.json', 'verified-records.json', 'verified-pattern.json', 'verified-traces.json']:
        assert json.loads((root / name).read_text()) == json.loads((HERE / name).read_text())
