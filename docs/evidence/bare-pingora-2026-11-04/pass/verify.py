#!/usr/bin/env python3
"""Verify actual framework/Edge inputs and all OFF-only wire controls."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import sys
import tempfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1]))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('download', type=Path)
parser.add_argument('edge_binaries', type=Path, help='original header-node binary artifact download')
args = parser.parse_args()
for name in ['artifact.json', 'manifest.json']:
    assert json.loads((args.download / name).read_text()) == json.loads((HERE / name).read_text())
root = verify_archive(args.download, '3b25c01f2b0d40ad90beb20a41e7cff917e54510')
binaries = verify_archive(args.edge_binaries, '4171ea700386d8ed7c755255c7eeccb7e804d644')
build = json.loads((root / 'build-metadata.json').read_text())
assert build['source'] == '3b25c01f2b0d40ad90beb20a41e7cff917e54510'
assert build['edge_runtime'] == 'ba474745fae3c08332d9e0862d9d5650550664ff'
for name, expected in build['expected_inputs'].items():
    path = binaries / ('cache-range-private/protocol-state-client' if name == 'protocol-state-client' else 'cache-range-binaries/after/' + name)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected
for name, expected in build['sources'].items():
    assert hashlib.sha256((root / 'source' / name).read_bytes()).hexdigest() == expected
pattern = r'\[\[package\]\]\nname = "([^"]+)"\n(version = "[^"]+"\nsource = "[^"]+"\nchecksum = "[^"]+"\n)'
assert set(re.findall(pattern, (root / 'source/Cargo.lock').read_text())) <= set(re.findall(pattern, (root / 'source/root-Cargo.lock').read_text()))
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    helper = workspace / 'scripts/perf/http-state/verify.py'
    helper.parent.mkdir(parents=True)
    shutil.copy2(HERE / 'http_verify.py', helper)
    spec = importlib.util.spec_from_file_location('frozen_bare_suite', HERE / 'frozen_suite.py')
    suite = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(suite)
    suite.ROOT, suite.PRIVATE, suite.OUT = workspace, root / 'records', root
    suite.verify()
for name in ['verified-records.json', 'verified-summary.json']:
    assert json.loads((root / name).read_text()) == json.loads((HERE / name).read_text())
spec = importlib.util.spec_from_file_location('bare_cpu_budget', HERE / 'cpu_budget.py')
budget = importlib.util.module_from_spec(spec)
spec.loader.exec_module(budget)
assert budget.inspect(root) == json.loads((HERE / 'cpu-budget.json').read_text())
