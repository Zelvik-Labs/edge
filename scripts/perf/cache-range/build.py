#!/usr/bin/env python3
"""Build both immutable production variants before starting any measurement."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path.cwd()
BASE = 'a4e95a239abb82420f65d79f813654ba67fedad9'
OUT = ROOT / '.local/cache-range-binaries'
LOGS = ROOT / '.local/cache-range-private'


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    source = command('git', 'rev-parse', 'HEAD')
    assert not command('git', 'status', '--porcelain', '--untracked-files=no')
    changes = command('git', 'diff', '--name-only', BASE, source).splitlines()
    runtime_changes = [p for p in changes if p.startswith(('src/', 'crates/', 'vendor/', 'pingora-main/'))
                       or p in ['Cargo.toml', 'Cargo.lock', 'build.rs', 'rust-toolchain.toml', '.cargo/config.toml']]
    assert sorted(runtime_changes) == ['src/cache/partial.rs', 'src/proxy.rs'], runtime_changes
    OUT.mkdir(parents=True, exist_ok=False)
    LOGS.mkdir(parents=True, exist_ok=True)
    metadata = {'harness_source': source, 'baseline': BASE, 'runtime_changes': runtime_changes,
                'rustc': command('rustc', '-vV'), 'rustflags': os.environ['RUSTFLAGS'],
                'variants': {}, 'builds_complete_before_measurement': False}
    try:
        for variant, revision in [('before', BASE), ('after', source)]:
            subprocess.run(['git', 'checkout', '--detach', revision], cwd=ROOT, check=True)
            directory = OUT / variant
            directory.mkdir()
            with (LOGS / (variant + '-build.log')).open('w') as log:
                subprocess.run(['cargo', 'build', '--locked', '--release', '--bin', 'Zelvik-Edge',
                                '--example', 'issue34-controller'], cwd=ROOT,
                               stdout=log, stderr=subprocess.STDOUT, check=True)
            binaries = {}
            for relative in ['Zelvik-Edge', 'examples/issue34-controller']:
                path = ROOT / 'target/release' / relative
                dest = directory / Path(relative).name
                shutil.copy2(path, dest)
                binaries[dest.name] = {'bytes': dest.stat().st_size, 'sha256': sha(dest)}
            metadata['variants'][variant] = {'source': revision, 'binaries': binaries,
                'source_hashes': {p: sha(ROOT / p) for p in [*runtime_changes, 'Cargo.lock', 'Cargo.toml']}}
            (OUT / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    finally:
        subprocess.run(['git', 'checkout', '--detach', source], cwd=ROOT, check=True)
    metadata['builds_complete_before_measurement'] = True
    (OUT / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    shutil.copy2(OUT / 'build-metadata.json', LOGS / 'build-metadata.json')


if __name__ == '__main__':
    main()
