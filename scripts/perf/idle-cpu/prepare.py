#!/usr/bin/env python3
"""Reuse the actual matched Edge/controller/client and old bare Pingora ELF."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path.cwd()
OUT = ROOT / '.local/idle-cpu-ci'
EXPECTED = {'Zelvik-Edge': '28e988dbe83f90b9cd5e7f50f6346d0bd0293b4f320bca0a80bf0193fff4b12e',
            'issue34-controller': '1a85adaec923f45c490b2bcc949adb35c562241e01024b3020aae1c99486b6b2',
            'protocol-state-client': '368312b058eb6a98373260a6b4977beb1655744418406c7c1c1bcedce92b3c1b',
            'bare-pingora-proxy': '1a5439941c53dc32044b665013add230ac8a2062ab34e72f92dfde7799d093c2'}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    OUT.mkdir(parents=True, exist_ok=False)
    bins = ROOT / '.local/idle-cpu-binaries'
    bins.mkdir()
    header = ROOT / '.local/architecture-header-binaries/cache-range-binaries'
    metadata = json.loads((header / 'build-metadata.json').read_text())
    assert metadata['variants']['after']['source'] == 'ba474745fae3c08332d9e0862d9d5650550664ff'
    bare = ROOT / '.local/framework-profile-input'
    assert json.loads((bare / 'build-metadata.json').read_text())['source'] == '3b25c01f2b0d40ad90beb20a41e7cff917e54510'
    for name, expected in EXPECTED.items():
        source = (bare / name if name == 'bare-pingora-proxy' else
                  ROOT / '.local/architecture-profile-binaries/.local/http-state-ci/protocol-state-client'
                  if name == 'protocol-state-client' else header / 'after' / name)
        assert sha(source) == expected
        target = (OUT if name == 'bare-pingora-proxy' else bins) / name
        shutil.copy2(source, target)
        target.chmod(0o755)
    source_dir = OUT / 'source'
    source_dir.mkdir()
    sources = {}
    for path in (ROOT / 'scripts/perf/idle-cpu').glob('*'):
        if path.is_file():
            shutil.copy2(path, source_dir / path.name)
            sources[path.name] = sha(path)
    (OUT / 'build-metadata.json').write_text(json.dumps(dict(
        source=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        expected_inputs=EXPECTED, sources=sources, no_compilation=True,
        edge_runtime='ba474745fae3c08332d9e0862d9d5650550664ff',
        scope='fixed-executable idle/load CPU diagnosis; no idle subtraction or new capacity claim'), indent=2) + '\n')


if __name__ == '__main__':
    main()
