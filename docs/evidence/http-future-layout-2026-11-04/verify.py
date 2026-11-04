#!/usr/bin/env python3
"""Verify the original real callback layout archive and actual release probe."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('download', type=Path)
args = parser.parse_args()
source = 'c5ebb91a76afe4855993f4822b4ac0657e85f18b'
root = verify_archive(args.download, source)
data = root / '.local/http-future-layout-ci'
metadata = json.loads((data / 'metadata.json').read_text())
assert metadata == json.loads((HERE / 'metadata.json').read_text())
assert metadata['source'] == source
assert metadata['example_sha256'] == hashlib.sha256((HERE / 'example.rs').read_bytes()).hexdigest()
assert metadata['binary_sha256'] == hashlib.sha256((root / 'target/release/examples/http-future-layout').read_bytes()).hexdigest()
layout = json.loads((data / 'layout.json').read_text())
assert layout == json.loads((HERE / 'layout.json').read_text())
assert len(layout['callbacks']) == 6
assert all(row['alignment'] == 8 and row['frame_bytes'] > 0 for row in layout['callbacks'].values())
print(json.dumps(layout, indent=2))
