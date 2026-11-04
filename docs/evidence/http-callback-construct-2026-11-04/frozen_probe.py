#!/usr/bin/env python3
"""Measure real callback creation/drop separately from request execution."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/http-callback-layout-ci'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cpu_snapshot(cpu):
    return list(map(int, next(row for row in Path('/proc/stat').read_text().splitlines()
                             if row.startswith(f'cpu{cpu} ')).split()[1:]))


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    for name in ['examples/http-future-layout.rs', 'benches/support/http_callback_construct.rs']:
        shutil.copy2(ROOT / name, source / Path(name).name)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'scope': 'unpolled real callback construction/drop only; not request execution, allocation counts or node capacity',
                'rustc': subprocess.check_output(['rustc', '-vV'], text=True), 'rustflags': os.environ['RUSTFLAGS'],
                'sources': {p.name: sha(p) for p in source.iterdir()}}
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    for label, command in [('clippy', ['cargo', 'clippy', '--locked', '--example', 'http-future-layout',
                                      '--', '-D', 'clippy::correctness', '-D', 'clippy::suspicious']),
                           ('build', ['cargo', 'build', '--locked', '--release', '--example', 'http-future-layout'])]:
        with (OUT / (label + '.log')).open('w') as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
    binary = OUT / 'probe'
    shutil.copy2(ROOT / 'target/release/examples/http-future-layout', binary)
    metadata['binary_sha256'] = sha(binary)
    cpu = min(os.sched_getaffinity(0))
    metadata['cpu'] = cpu
    metadata['cpu_before'] = cpu_snapshot(cpu)
    for comm in Path('/proc').glob('[0-9]*/comm'):
        try:
            assert comm.read_text().strip() not in ['rustc', 'mbx-rustc'], 'overlapping compiler'
        except FileNotFoundError:
            continue
    with (OUT / 'layout.json').open('w') as output, (OUT / 'probe.log').open('w') as log:
        result = subprocess.run(['taskset', '-c', str(cpu), str(binary)], stdout=output, stderr=log)
    metadata['run_exit'] = result.returncode
    metadata['cpu_after'] = cpu_snapshot(cpu)
    delta = [b - a for a, b in zip(metadata['cpu_before'], metadata['cpu_after'])]
    assert all(v >= 0 for v in delta[:8]) and sum(delta[:8]) > 0
    metadata['host_steal_fraction'] = delta[7] / sum(delta[:8])
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    verify()


def verify():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    assert metadata['run_exit'] == 0 and sha(OUT / 'probe') == metadata['binary_sha256']
    for name, expected in metadata['sources'].items():
        assert sha(OUT / 'source' / name) == expected
    layout = json.loads((OUT / 'layout.json').read_text())
    assert layout['allocator'] == 'mimalloc, as in the production executable'
    names = ['early_request_filter', 'request_filter', 'upstream_peer',
             'upstream_response_filter', 'response_filter', 'logging']
    assert set(layout['callbacks']) == set(names)
    rows = {}
    for name, data in layout['callbacks'].items():
        blocks = data['construction_drop_blocks']
        assert len(blocks) == 20 and [r['block'] for r in blocks] == list(range(20))
        assert all(r['iterations'] == 100000 and r['nanoseconds'] > 0 for r in blocks)
        values = [r['nanoseconds'] / r['iterations'] for r in blocks]
        assert data['frame_bytes'] > 0 and data['alignment'] > 0
        rows[name] = {'frame_bytes': data['frame_bytes'], 'alignment': data['alignment'],
                      'median_ns': statistics.median(values), 'min_ns': min(values), 'max_ns': max(values)}
    summary = {'source': metadata['source'], 'scope': metadata['scope'], 'raw_blocks': 120,
               'measured_constructions': 12000000, 'host_steal_fraction': metadata['host_steal_fraction'],
               'callbacks': rows}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'verify'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        OUT = args.directory.resolve()
    globals()[args.mode]()
