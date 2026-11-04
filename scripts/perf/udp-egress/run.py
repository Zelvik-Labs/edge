#!/usr/bin/env python3
"""Compare exact channel helpers with unchanged errors, ownership and capacity."""
import argparse
from collections import defaultdict
import csv
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/udp-egress-ci'
BASE = 'c31047359d14ec1a2596d244be8690960b49d7f8'
CASES = [f'{kind}-{size}-{state}' for kind, size in
         [('copied', 64), ('copied', 1200), ('copied', 65507), ('owned', 1200)]
         for state in ['ready', 'full', 'closed']]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    baseline = subprocess.check_output(['git', 'show', BASE + ':src/udp_proxy.rs'], text=True)
    (source / 'original-full.rs').write_text(baseline)
    start = baseline.index('#[derive(Clone, Debug, PartialEq, Eq)]\npub struct DownstreamUdpDatagram')
    end = baseline.index('#[derive(Debug)]\npub struct ChannelUdpDownstreamSender', start)
    clock = baseline[baseline.index('static UDP_ACTIVITY_EPOCH:'):baseline.index('pub(crate) fn udp_activity_is_alive(')]
    (source / 'common.rs').write_text(baseline[start:end] + '\n' + clock)
    old = baseline[end:baseline.index('fn udp_socket_downstream(', end)]
    new = (ROOT / 'benches/support/udp_egress_candidate.rs').read_text()
    methods = '''
impl Sender for ChannelUdpDownstreamSender {
    fn new(listen_addr: SocketAddr, tx: mpsc::Sender<DownstreamUdpDatagram>) -> Self { Self { listen_addr, tx } }
    fn copied(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> { self.try_send_to(data, target) }
    fn owned(&self, data: Bytes, target: SocketAddr) -> io::Result<usize> { self.try_send_owned_to(data, target) }
}
'''
    for name, code in [('baseline', old), ('candidate', new)]:
        (source / (name + '.rs')).write_text('use super::*;\n' + code + methods)
    allocator = (ROOT / 'benches/support/sni_client_hello_probe.rs').read_text()
    (source / 'allocator.rs').write_text(allocator[allocator.index('#[cfg(feature = "count-allocations")]'):allocator.index('\nfn record(payload:')])
    shutil.copy2(ROOT / 'benches/support/udp_egress_probe.rs', source / 'main.rs')
    manifest = (ROOT / 'scripts/perf/udp-egress/Cargo.toml').read_text().replace('../../../benches/support/udp_egress_probe.rs', 'main.rs')
    (source / 'Cargo.toml').write_text(manifest)
    shutil.copy2(ROOT / 'scripts/perf/udp-egress/Cargo.lock', source / 'Cargo.lock')
    for name in ['bytes', 'tokio']:
        pattern = r'\[\[package\]\]\nname = "' + name + r'"\n((?:[^\n]+\n){3})'
        assert re.search(pattern, (ROOT / 'Cargo.lock').read_text()).group(1) == re.search(pattern, (source / 'Cargo.lock').read_text()).group(1)
    target = ROOT / '.local/udp-egress-target'
    for mode, features in [('timing', []), ('allocations', ['--features', 'count-allocations'])]:
        for operation, options in [('clippy', ['--', '-D', 'warnings']), ('build', ['--release'])]:
            with (OUT / (mode + '-' + operation + '.log')).open('w') as log:
                subprocess.run(['cargo', operation, '--locked', '--manifest-path', str(source / 'Cargo.toml'),
                                '--target-dir', str(target), *features, *options], stdout=log, stderr=subprocess.STDOUT, check=True)
        shutil.copy2(target / 'release/udp-egress-probe', OUT / mode)
    metadata = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'baseline': BASE, 'scope': 'bounded downstream send component; no node throughput or real AF_XDP ring claim',
                'rustc': subprocess.check_output(['rustc', '-vV'], text=True), 'rustflags': os.environ['RUSTFLAGS'],
                'binaries': {name: sha(OUT / name) for name in ['timing', 'allocations']},
                'sources': {p.name: sha(p) for p in source.iterdir()}, 'runs': []}
    (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')


def snapshot(cpu):
    return list(map(int, next(row for row in Path('/proc/stat').read_text().splitlines()
                             if row.startswith(f'cpu{cpu} ')).split()[1:]))


def measure():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    cpu = min(os.sched_getaffinity(0))
    metadata['cpu'] = cpu
    for variant in ['baseline', 'candidate']:
        for mode in ['controls', 'allocations']:
            with (OUT / (variant + '-' + mode + '.tsv')).open('w') as log:
                subprocess.run(['taskset', '-c', str(cpu), str(OUT / ('allocations' if mode == 'allocations' else 'timing')),
                                mode, variant], stdout=log, stderr=subprocess.STDOUT, check=True)
    for repeat in range(8):
        for variant in (['baseline', 'candidate'] if repeat % 2 == 0 else ['candidate', 'baseline']):
            for comm in Path('/proc').glob('[0-9]*/comm'):
                try:
                    assert comm.read_text().strip() not in ['rustc', 'mbx-rustc'], 'overlapping compiler'
                except FileNotFoundError:
                    continue
            file = f'round-{repeat}-{variant}.tsv'
            before = snapshot(cpu)
            with (OUT / file).open('w') as log:
                subprocess.run(['taskset', '-c', str(cpu), str(OUT / 'timing'), 'timing', variant], stdout=log, stderr=subprocess.STDOUT, check=True)
            after = snapshot(cpu)
            delta = [b - a for a, b in zip(before, after)]
            assert all(v >= 0 for v in delta[:8]) and sum(delta[:8]) > 0
            metadata['runs'].append({'round': repeat, 'variant': variant, 'file': file,
                                    'cpu_before': before, 'cpu_after': after,
                                    'host_steal_fraction': delta[7] / sum(delta[:8])})
            (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    verify()


def read(path):
    with path.open() as stream:
        return list(csv.DictReader(stream, delimiter='\t'))


def verify():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    for name, expected in metadata['binaries'].items():
        assert sha(OUT / name) == expected
    for name, expected in metadata['sources'].items():
        assert sha(OUT / 'source' / name) == expected
    controls, allocations = {}, {}
    for variant in ['baseline', 'candidate']:
        controls[variant] = read(OUT / (variant + '-controls.tsv'))
        assert [r['case'] for r in controls[variant]] == CASES
        assert all(r['status'] == 'PASS' and r['recovered'] == r['closed'] == 'true' for r in controls[variant])
        rows = read(OUT / (variant + '-allocations.tsv'))
        assert [r['case'] for r in rows] == CASES
        allocations[variant] = {r['case']: {k: int(v) for k, v in r.items() if k != 'case'} for r in rows}
    assert controls['baseline'] == controls['candidate']
    for case in CASES:
        old, new = [allocations[v][case] for v in ['baseline', 'candidate']]
        assert old['iterations'] == new['iterations'] == 1000
        assert old['reallocations'] == new['reallocations'] == 0
        rejected_copy = case.startswith('copied') and not case.endswith('ready')
        assert old['allocations'] - new['allocations'] == (1000 if rejected_copy else 0)
        assert old['bytes'] - new['bytes'] == (1000 * int(case.split('-')[1]) if rejected_copy else 0)
    assert [(r['round'], r['variant']) for r in metadata['runs']] == [
        (i, v) for i in range(8) for v in (['baseline', 'candidate'] if i % 2 == 0 else ['candidate', 'baseline'])]
    values = defaultdict(dict)
    for run in metadata['runs']:
        rows = read(OUT / run['file'])
        assert len(rows) == 240
        for case in CASES:
            selected = [r for r in rows if r['case'] == case]
            assert len(selected) == 20 and [int(r['block']) for r in selected] == list(range(20))
            assert all(int(r['iterations']) > 0 and int(r['nanoseconds']) > 0 for r in selected)
            values[case][run['round'], run['variant']] = sum(int(r['nanoseconds']) for r in selected) / sum(int(r['iterations']) for r in selected)
    summary = {'source': metadata['source'], 'baseline': BASE, 'scope': metadata['scope'],
               'controls': controls, 'allocations': allocations, 'cases': {},
               'max_host_steal_fraction': max(r['host_steal_fraction'] for r in metadata['runs'])}
    for case in CASES:
        changes = [(values[case][i, 'candidate'] / values[case][i, 'baseline'] - 1) * 100 for i in range(8)]
        summary['cases'][case] = {'baseline_ns': statistics.median(values[case][i, 'baseline'] for i in range(8)),
                                 'candidate_ns': statistics.median(values[case][i, 'candidate'] for i in range(8)),
                                 'paired_percent_changes': changes, 'paired_median_percent': statistics.median(changes)}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k not in ['controls', 'allocations']}, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'measure', 'verify'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        OUT = args.directory.resolve()
    globals()[args.mode]()
