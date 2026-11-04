#!/usr/bin/env python3
"""Verify first diagnostic archives, actual binaries, partial records and causes."""
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
parser.add_argument('--phase', action='store_true')
args = parser.parse_args()
stage = HERE / 'phase' if args.phase else HERE
source = '1ff0c14215c322b7c46852aedbce4e38088f18ad' if args.phase else '2ca3975b84b802957d85d27248c4411784daf473'
root = verify_archive(args.evidence, source)
binaries = verify_archive(args.binaries, source)
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    helper = workspace / 'scripts/perf/http-state/verify.py'
    helper.parent.mkdir(parents=True)
    shutil.copy2(stage / 'http_verify.py', helper)
    spec = importlib.util.spec_from_file_location('frozen_diagnosis', stage / 'frozen_verify.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = workspace
    module.verify(root, binaries / 'quinn-diagnostic-binaries')
assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((stage / 'verified-summary.json').read_text())
