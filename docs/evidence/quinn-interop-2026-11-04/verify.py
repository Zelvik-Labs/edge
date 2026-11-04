#!/usr/bin/env python3
"""Verify the first incomplete client-control run without modifying its archive."""
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
parser.add_argument('--ca-only', action='store_true')
parser.add_argument('--groups', action='store_true')
args = parser.parse_args()
assert not (args.ca_only and args.groups)
stage = HERE / 'groups' if args.groups else HERE / 'ca-only' if args.ca_only else HERE
source = ('186fedc728ea868afbeaea37575df2698fb0cfe7' if args.groups else
          'b41985942fb5c97ae0ee750b7c60de56badd8a82' if args.ca_only else
          '9390bfbaea6e7224a1a8b13bdd29bc181821cd3a')
original = verify_archive(args.evidence, source)
binaries = verify_archive(args.binaries, source)
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    helper = workspace / 'scripts/perf/http-state/verify.py'
    helper.parent.mkdir(parents=True)
    shutil.copy2(stage / 'http_verify.py', helper)
    root = workspace / 'evidence'
    shutil.copytree(original, root)
    spec = importlib.util.spec_from_file_location('frozen_interop', stage / 'frozen_verify.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = workspace
    module.verify(root, binaries / 'quinn-diagnostic-binaries')
    assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((stage / 'verified-summary.json').read_text())
    if args.groups:
        spec = importlib.util.spec_from_file_location('group_pattern', stage / 'pattern.py')
        pattern = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(pattern)
        pattern.verify(root)
        assert json.loads((root / 'verified-pattern.json').read_text()) == json.loads((stage / 'verified-pattern.json').read_text())
