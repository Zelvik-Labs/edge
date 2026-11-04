#!/usr/bin/env python3
"""Collect wire controls and failures without TLS keys or node snapshots."""
import hashlib
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/pingora-buffers-private'
OUT = ROOT / '.local/pingora-buffers-ci'
OUT.mkdir(parents=True, exist_ok=True)
exact = {'suite.json', 'manifest.json', 'payload', 'origin-connections.json', 'tls-controls.json', 'proxy.conf', 'origin.conf', 'buffer-identity.json'}
suffixes = ('.client.tsv.gz', '.client.tsv', '.stderr', '.command.json', '.driver.log', '.body', '.headers')
files = []
if PRIVATE.exists():
    for path in PRIVATE.rglob('*'):
        if path.is_file() and (path.name in exact or path.name.endswith(suffixes)):
            relative = path.relative_to(PRIVATE)
            output = OUT / 'records' / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, output)
            files.append({'path': str(Path('records') / relative), 'bytes': output.stat().st_size,
                          'sha256': hashlib.sha256(output.read_bytes()).hexdigest()})
(OUT / 'retained-manifest.json').write_text(json.dumps({'files': files,
    'excluded': ['TLS private keys', 'runtime snapshots and node stores', 'duplicate immutable Edge inputs']}, indent=2) + '\n')
