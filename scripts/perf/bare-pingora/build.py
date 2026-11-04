#!/usr/bin/env python3
"""Build the OFF-only control with unchanged vendor code and locked packages."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path.cwd()
OUT = ROOT / '.local/bare-pingora-ci'
EDGE_SOURCE = 'ba474745fae3c08332d9e0862d9d5650550664ff'
EXPECTED = {'Zelvik-Edge': '28e988dbe83f90b9cd5e7f50f6346d0bd0293b4f320bca0a80bf0193fff4b12e',
            'issue34-controller': '1a85adaec923f45c490b2bcc949adb35c562241e01024b3020aae1c99486b6b2',
            'protocol-state-client': '368312b058eb6a98373260a6b4977beb1655744418406c7c1c1bcedce92b3c1b'}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    OUT.mkdir(parents=True, exist_ok=False)
    subprocess.run(['git', 'diff', '--exit-code', EDGE_SOURCE, '--', 'pingora-main', 'Cargo.lock'], check=True)
    source = OUT / 'source'
    source.mkdir()
    for name in ['benches/support/bare_pingora_proxy.rs', 'scripts/perf/bare-pingora/Cargo.toml', 'scripts/perf/bare-pingora/Cargo.lock']:
        shutil.copy2(ROOT / name, source / Path(name).name)
    root_lock = (ROOT / 'Cargo.lock').read_text()
    (source / 'root-Cargo.lock').write_text(root_lock)
    pattern = r'\[\[package\]\]\nname = "([^"]+)"\n(version = "[^"]+"\nsource = "[^"]+"\nchecksum = "[^"]+"\n)'
    assert set(re.findall(pattern, (source / 'Cargo.lock').read_text())) <= set(re.findall(pattern, root_lock))
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'edge_runtime': EDGE_SOURCE, 'vendor_unchanged_since_edge_runtime': True,
                'vendor_tree': subprocess.check_output(['git', 'rev-parse', 'HEAD:pingora-main'], text=True).strip(),
                'scope': 'OFF-only minimal Pingora control; not production Edge or feature-equivalent replacement',
                'allocator': 'mimalloc', 'tls': 'rustls AWS-LC', 'rustflags': os.environ['RUSTFLAGS'],
                'rustc': subprocess.check_output(['rustc', '-vV'], text=True),
                'sources': {p.name: sha(p) for p in source.iterdir()}, 'expected_inputs': EXPECTED}
    (OUT / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    manifest = str(ROOT / 'scripts/perf/bare-pingora/Cargo.toml')
    target = ROOT / '.local/bare-pingora-target'
    for mode, extra in [('clippy', ['--', '-D', 'warnings']), ('build', ['--release'])]:
        with (OUT / (mode + '.log')).open('w') as log:
            subprocess.run(['cargo', mode, '--locked', '--manifest-path', manifest,
                '--target-dir', str(target), *extra], stdout=log, stderr=subprocess.STDOUT, check=True)
    binary = OUT / 'bare-pingora-proxy'
    shutil.copy2(target / 'release/bare-pingora-proxy', binary)
    metadata['bare_binary'] = {'bytes': binary.stat().st_size, 'sha256': sha(binary)}
    metadata['version'] = subprocess.check_output([str(binary), '--version'], text=True)
    bins = ROOT / '.local/bare-pingora-binaries'
    bins.mkdir()
    headers = ROOT / '.local/architecture-header-binaries/cache-range-binaries'
    build = json.loads((headers / 'build-metadata.json').read_text())
    assert build['variants']['after']['source'] == EDGE_SOURCE
    for name in EXPECTED:
        path = headers / 'after' / name if name != 'protocol-state-client' else ROOT / '.local/architecture-profile-binaries/.local/http-state-ci/protocol-state-client'
        assert sha(path) == EXPECTED[name], name
        shutil.copy2(path, bins / name)
        (bins / name).chmod((bins / name).stat().st_mode | 0o111)
    metadata['all_compilation_complete_before_measurement'] = True
    (OUT / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')


if __name__ == '__main__':
    main()
