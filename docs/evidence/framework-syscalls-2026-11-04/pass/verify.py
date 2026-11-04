#!/usr/bin/env python3
"""Recompute all sixteen counts and verify the actual four-product input ELFs."""
import argparse
import hashlib
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
parser.add_argument('download', type=Path)
parser.add_argument('edge_binaries', type=Path)
parser.add_argument('bare_download', type=Path)
args = parser.parse_args()
source = '76610685fe3b62947f931a670d6c1a2ec07b066d'
for name in ['artifact.json', 'manifest.json']:
    assert json.loads((args.download / name).read_text()) == json.loads((HERE / name).read_text())
root = verify_archive(args.download, source)
edge = verify_archive(args.edge_binaries, '4171ea700386d8ed7c755255c7eeccb7e804d644')
bare = verify_archive(args.bare_download, '3b25c01f2b0d40ad90beb20a41e7cff917e54510')
for name in ['suite.json', 'provenance.json', 'bare-build-metadata.json']:
    assert json.loads((root / name).read_text()) == json.loads((HERE / name).read_text())
provenance = json.loads((root / 'provenance.json').read_text())
for relative, expected in provenance['expected_binaries'].items():
    path = edge / ('cache-range-private/protocol-state-client' if relative.endswith('protocol-state-client')
                   else 'cache-range-binaries/after/' + Path(relative).name)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected
build = json.loads((root / 'bare-build-metadata.json').read_text())
assert build == json.loads((bare / 'build-metadata.json').read_text())
assert hashlib.sha256((bare / 'bare-pingora-proxy').read_bytes()).hexdigest() == build['bare_binary']['sha256']
with tempfile.TemporaryDirectory() as directory:
    workspace = Path(directory)
    for name, target in [('http_verify.py', 'scripts/perf/http-state/verify.py'),
                         ('syscall_profile.py', 'scripts/perf/afxdp-node/syscall_profile.py')]:
        path = workspace / target
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(HERE / name, path)
    spec = importlib.util.spec_from_file_location('frozen_syscall_counts', HERE / 'frozen_probe.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT, module.PRIVATE = workspace, root
    module.verify()
assert json.loads((root / 'verified-syscalls.json').read_text()) == json.loads((HERE / 'verified-syscalls.json').read_text())
