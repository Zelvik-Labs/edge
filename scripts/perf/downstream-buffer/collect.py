#!/usr/bin/env python3
"""Publish an explicit evidence allow-list; runtime overrides contain TLS keys."""
import hashlib
import json
from pathlib import Path
import shutil

ROOT = Path.cwd()
PRIVATE = ROOT / '.local/cache-range-private'
PUBLIC = ROOT / '.local/cache-range-ci'
PUBLIC.mkdir(parents=True, exist_ok=True)
exact = {'build-metadata.json', 'suite.json', 'range-controls.json', 'manifest.json', 'payload',
         'curl-version.txt', 'packages.txt', 'kernel.txt', 'python-tests.log',
         'before-build.log', 'after-build.log', 'verified-records.json', 'verified-summary.json',
         'origin-readiness.json', 'binary-artifact.json', 'tls-controls.json'}
suffixes = ('.client.tsv.gz', '.client.tsv', '.stderr', '.command.json', '.body', '.headers')
retained = []
for path in PRIVATE.rglob('*'):
    if not path.is_file() or not (path.name in exact or path.name.endswith(suffixes)):
        continue
    relative = path.relative_to(PRIVATE)
    target = PUBLIC / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, target)
    retained.append({'path': str(relative), 'bytes': target.stat().st_size,
                     'sha256': hashlib.sha256(target.read_bytes()).hexdigest()})
(PUBLIC / 'retained-manifest.json').write_text(json.dumps({'files': retained,
    'excluded': ['runtime overrides', 'TLS private material', 'controller stores', 'node logs']}, indent=2) + '\n')
print(json.dumps({'retained_files': len(retained)}))
