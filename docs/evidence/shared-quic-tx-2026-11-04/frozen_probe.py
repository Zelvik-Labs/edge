#!/usr/bin/env python3
"""Build the optional TX candidate and retain real-kernel controls."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/shared-quic-tx-ci'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    shutil.copy2(ROOT / 'benches/support/quic_shared_tx_candidate.rs', source / 'candidate.rs')
    shutil.copy2(ROOT / 'benches/support/quic_shared_tx_probe.rs', source / 'main.rs')
    manifest = (ROOT / 'scripts/perf/shared-quic-tx/Cargo.toml').read_text().replace(
        '../../../benches/support/quic_shared_tx_probe.rs', 'main.rs')
    (source / 'Cargo.toml').write_text(manifest)
    shutil.copy2(ROOT / 'scripts/perf/shared-quic-tx/Cargo.lock', source / 'Cargo.lock')
    root_lock = (ROOT / 'Cargo.lock').read_text()
    for name in ['libc', 'socket2', 'tokio', 'quinn-udp']:
        pattern = r'\[\[package\]\]\nname = "' + name + r'"\n((?:[^\n]+\n){3})'
        assert re.search(pattern, root_lock).group(1) == re.search(pattern, (source / 'Cargo.lock').read_text()).group(1)
    target = ROOT / '.local/shared-quic-tx-target'
    for mode, extra in [('clippy', ['--', '-D', 'warnings']), ('build', ['--release'])]:
        with (OUT / (mode + '.log')).open('w') as log:
            subprocess.run(['cargo', mode, '--locked', '--manifest-path', str(source / 'Cargo.toml'),
                '--target-dir', str(target), *extra], stdout=log, stderr=subprocess.STDOUT, check=True)
    binary = OUT / 'shared-quic-tx-probe'
    shutil.copy2(target / 'release/shared-quic-tx-probe', binary)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'rustc': subprocess.check_output(['rustc', '-vV'], text=True), 'rustflags': os.environ['RUSTFLAGS'],
        'kernel': subprocess.check_output(['uname', '-a'], text=True).strip(),
        'scope': 'Linux loopback TX candidate correctness; no production integration or throughput claim',
        'binary_sha256': sha(binary), 'sources': {p.name: sha(p) for p in source.iterdir()}}
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    with (OUT / 'controls.log').open('w') as log:
        result = subprocess.run([str(binary)], stdout=log, stderr=subprocess.STDOUT)
    metadata['run_exit'] = result.returncode
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    verify()


def verify():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    assert sha(OUT / 'shared-quic-tx-probe') == metadata['binary_sha256']
    for name, digest in metadata['sources'].items():
        assert sha(OUT / 'source' / name) == digest
    assert metadata['run_exit'] == 0
    lines = (OUT / 'controls.log').read_text().splitlines()
    assert len(lines) == 3
    for family, line in zip(['IPv4', 'IPv6'], lines):
        match = re.fullmatch(r'family=(\w+) segments=(\d+) gro=false fragmentation=(true|false) delivered=16x1200 initial_readiness=PASS oversize_errno=(\d+)', line)
        assert match and match[1] == family and int(match[2]) >= 8 and int(match[4]) > 0
    assert lines[-1] == 'PASS: candidate only; no production integration or throughput claim'
    (OUT / 'verified-summary.json').write_text(json.dumps({'source': metadata['source'],
        'scope': metadata['scope'], 'controls': lines, 'result': 'PASS'}, indent=2) + '\n')
    print('\n'.join(lines))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'verify'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        OUT = args.directory.resolve()
    globals()[args.mode]()
