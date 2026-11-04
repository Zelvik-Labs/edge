#!/usr/bin/env python3
"""Recompute the twelve immutable Edge/bare-Pingora CPU profiles."""
import argparse
import hashlib
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
parser.add_argument('download', type=Path)
parser.add_argument('edge_binaries', type=Path)
args = parser.parse_args()
for name in ['artifact.json', 'manifest.json']:
    assert json.loads((args.download / name).read_text()) == json.loads((HERE / name).read_text())
root = verify_archive(args.download, '95906b87b190f1756bb758be4904a58172e7f467')
binaries = verify_archive(args.edge_binaries, '4171ea700386d8ed7c755255c7eeccb7e804d644')
provenance = json.loads((root / 'provenance.json').read_text())
for relative, expected in provenance['expected_binaries'].items():
    path = binaries / ('cache-range-private/protocol-state-client' if relative.endswith('protocol-state-client') else 'cache-range-binaries/after/' + Path(relative).name)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    for name, relative in [('http_verify.py', 'scripts/perf/http-state/verify.py'), ('profile_summary.py', 'scripts/perf/summarize_architecture.py')]:
        path = workspace / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(HERE / name, path)
    spec = importlib.util.spec_from_file_location('frozen_framework', HERE / 'frozen_probe.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT, module.PRIVATE = workspace, root
    module.verify()
assert json.loads((root / 'verified-profiles.json').read_text()) == json.loads((HERE / 'verified-profiles.json').read_text())
