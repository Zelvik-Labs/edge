#!/usr/bin/env python3
"""Verify post-cache CPU profiles against the original archives and binaries."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('evidence', type=Path)
parser.add_argument('binaries', type=Path)
parser.add_argument('client', type=Path)
parser.add_argument('--write-summary', type=Path)
args = parser.parse_args()
root = verify_archive(args.evidence, '8e0c4330caaef898d2c09f15f8e31400eded5033')
binaries = verify_archive(args.binaries, '4171ea700386d8ed7c755255c7eeccb7e804d644')
spec = importlib.util.spec_from_file_location('frozen_after_cache', HERE / 'frozen_inspect.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
provenance = json.loads((root / 'provenance.json').read_text())
for relative, expected in provenance['expected_binaries'].items():
    path = args.client if relative.endswith('protocol-state-client') else binaries / 'cache-range-binaries/after' / Path(relative).name
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected
result = module.inspect(root)
if args.write_summary:
    args.write_summary.write_text(json.dumps(result, indent=2) + '\n')
else:
    assert result == json.loads((HERE / 'attribution.json').read_text())
print(json.dumps({'result': 'PASS', 'transfers': result['successful'], 'samples': result['samples'],
                  'lost_samples': 0, 'actual_binaries_checked': True}))
