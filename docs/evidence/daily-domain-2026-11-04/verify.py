#!/usr/bin/env python3
"""Verify daily map archives, keeping failed workflow status distinct."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
sources = {'setup-failure': '3e3e2e5e5d178fa8ad89666bd18b86d4e4dc64be',
           'first-measure': '8486c1bec1633655af64e756473ecfa927e67ca5',
           'pass': '739aa162d367040393e1d6d91293af231e7233e6'}
parser.add_argument('stage', choices=sources)
parser.add_argument('download', type=Path)
args = parser.parse_args()
root = verify_archive(args.download, sources[args.stage])
if args.stage == 'setup-failure':
    assert json.loads((root / 'setup.json').read_text()) == json.loads((HERE / args.stage / 'setup.json').read_text())
    assert not (root / 'metadata.json').exists()
else:
    spec = importlib.util.spec_from_file_location('frozen_daily', HERE / 'frozen_probe.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.OUT = root
    module.verify()
    assert json.loads((root / 'verified-summary.json').read_text()) == json.loads((HERE / args.stage / 'verified-summary.json').read_text())
