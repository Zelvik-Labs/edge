#!/usr/bin/env python3
"""Build one Hyper control and retain every exact compiler and dependency input."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path.cwd()
OUT = ROOT / '.local/bare-hyper-ci'
EXPECTED = {'Zelvik-Edge': '28e988dbe83f90b9cd5e7f50f6346d0bd0293b4f320bca0a80bf0193fff4b12e',
    'issue34-controller': '1a85adaec923f45c490b2bcc949adb35c562241e01024b3020aae1c99486b6b2',
    'protocol-state-client': '368312b058eb6a98373260a6b4977beb1655744418406c7c1c1bcedce92b3c1b'}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    for name in ['benches/support/bare_hyper_proxy.rs', 'scripts/perf/bare-hyper/Cargo.toml',
                 'scripts/perf/bare-hyper/Cargo.lock', 'scripts/perf/bare-hyper/build.py']:
        target = source / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, target)
    shutil.copy2(ROOT / 'Cargo.lock', source / 'root-Cargo.lock')
    pattern = r'\[\[package\]\]\nname = "([^"]+)"\n(version = "[^"]+"\nsource = "[^"]+"\nchecksum = "[^"]+"\n)'
    assert set(re.findall(pattern, (ROOT / 'scripts/perf/bare-hyper/Cargo.lock').read_text())) <= set(re.findall(pattern, (ROOT / 'Cargo.lock').read_text()))
    bare = ROOT / '.local/framework-profile-input'
    pingora = json.loads((bare / 'build-metadata.json').read_text())
    assert pingora['source'] == '3b25c01f2b0d40ad90beb20a41e7cff917e54510'
    assert sha(bare / 'bare-pingora-proxy') == pingora['bare_binary']['sha256'] == '1a5439941c53dc32044b665013add230ac8a2062ab34e72f92dfde7799d093c2'
    shutil.copy2(bare / 'bare-pingora-proxy', OUT / 'bare-pingora-proxy')
    shutil.copy2(bare / 'build-metadata.json', OUT / 'bare-pingora-build-metadata.json')
    (OUT / 'bare-pingora-proxy').chmod(0o755)
    metadata = dict(source=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        edge_runtime='ba474745fae3c08332d9e0862d9d5650550664ff',
        scope='OFF-only five-product framework control; no Edge business feature equivalence or production replacement',
        allocator='mimalloc', tls='rustls AWS-LC', runtime='Tokio multi_thread with explicit worker count matching proxy CPU quota',
        rustflags=os.environ['RUSTFLAGS'], rustc=subprocess.check_output(['rustc', '-vV'], text=True),
        expected_inputs=EXPECTED, pingora_binary=pingora['bare_binary'],
        sources={str(path.relative_to(source)): sha(path) for path in source.rglob('*') if path.is_file()})
    def save():
        (OUT / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    save()
    manifest = str(ROOT / 'scripts/perf/bare-hyper/Cargo.toml')
    target = ROOT / '.local/bare-hyper-target'
    for mode, extra in [('clippy', ['--', '-D', 'warnings']), ('build', ['--release'])]:
        with (OUT / (mode + '.log')).open('w') as log:
            result = subprocess.run(['cargo', mode, '--locked', '--manifest-path', manifest,
                '--target-dir', str(target), *extra], stdout=log, stderr=subprocess.STDOUT)
        metadata[mode + '_exit'] = result.returncode
        save()
        assert result.returncode == 0, 'control build failed; original log retained'
    binary = OUT / 'bare-hyper-proxy'
    shutil.copy2(target / 'release/bare-hyper-proxy', binary)
    metadata['bare_binary'] = dict(bytes=binary.stat().st_size, sha256=sha(binary))
    metadata['version'] = subprocess.check_output([str(binary), '--version'], text=True)
    bins = ROOT / '.local/bare-hyper-binaries'
    bins.mkdir()
    header = ROOT / '.local/architecture-header-binaries/cache-range-binaries'
    build = json.loads((header / 'build-metadata.json').read_text())
    assert build['variants']['after']['source'] == metadata['edge_runtime']
    for name, expected in EXPECTED.items():
        path = header / 'after' / name if name != 'protocol-state-client' else ROOT / '.local/architecture-profile-binaries/.local/http-state-ci/protocol-state-client'
        assert sha(path) == expected
        shutil.copy2(path, bins / name)
        (bins / name).chmod(0o755)
    metadata['all_compilation_complete_before_measurement'] = True
    save()


if __name__ == '__main__':
    main()
