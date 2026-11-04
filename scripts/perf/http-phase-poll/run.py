#!/usr/bin/env python3
"""Measure actual ready request phases, retaining clock overhead separately."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/http-phase-poll-ci'
SCENARIOS = ['new-context', 'request-filter', 'phase-polls', 'clock-calibration']
PHASES = ['context', 'local_service', 'protocol_policy', 'early_response',
          'security_state', 'limits', 'lightweight_waf', 'body_waf', 'feature_policy', 'rewrite']
SCOPE = ('One parsed H1 loopback GET, minimal live route, mimalloc, current-thread Tokio; '
         'actual new_ctx/request_filter/private phase methods. Active poll wall time includes '
         'clock and instrumentation overhead and preemption; not node CPU, capacity, '
         'cache, upstream IO, early admission, response or logging lifecycle. '
         'Do not add phase percentages or subtract the clock floor as an exact correction.')


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(cpu):
    return list(map(int, next(line for line in Path('/proc/stat').read_text().splitlines()
                             if line.startswith(f'cpu{cpu} ')).split()[1:]))


def build():
    OUT.mkdir(parents=True, exist_ok=False)
    source = OUT / 'source'
    source.mkdir()
    paths = ['examples/http-phase-poll.rs', 'src/proxy.rs', 'src/config.rs',
             'Cargo.toml', 'Cargo.lock', 'scripts/perf/http-phase-poll/run.py']
    for name in paths:
        destination = source / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, destination)
    metadata = dict(source=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        scope=SCOPE, rustc=subprocess.check_output(['rustc', '-vV'], text=True),
        rustflags=os.environ['RUSTFLAGS'], features=['http-layout-probe'],
        sources={name: sha(source / name) for name in paths})
    def save():
        (OUT / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    save()
    commands = [('clippy', ['cargo', 'clippy', '--locked', '--features', 'http-layout-probe',
                 '--example', 'http-phase-poll', '--', '-D', 'clippy::correctness', '-D', 'clippy::suspicious']),
                ('build', ['cargo', 'build', '--locked', '--release', '--features', 'http-layout-probe',
                 '--example', 'http-phase-poll'])]
    for label, command in commands:
        with (OUT / (label + '.log')).open('w') as log:
            result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
        metadata[label + '_exit'] = result.returncode
        save()
        assert result.returncode == 0, f'{label} failed; original log retained'
    binary = OUT / 'probe'
    shutil.copy2(ROOT / 'target/release/examples/http-phase-poll', binary)
    metadata['binary_sha256'] = sha(binary)
    metadata['binary_bytes'] = binary.stat().st_size
    cpu = min(os.sched_getaffinity(0))
    metadata['cpu'] = cpu
    for comm in Path('/proc').glob('[0-9]*/comm'):
        try:
            assert comm.read_text().strip() not in ['rustc', 'mbx-rustc'], 'overlapping compiler'
        except FileNotFoundError:
            continue
    metadata['cpu_before'] = snapshot(cpu)
    save()
    with (OUT / 'blocks.jsonl').open('w') as output, (OUT / 'probe.log').open('w') as log:
        result = subprocess.run(['taskset', '-c', str(cpu), str(binary)], stdout=output,
                                stderr=log, timeout=300)
    metadata['run_exit'] = result.returncode
    metadata['cpu_after'] = snapshot(cpu)
    save()
    verify()


def describe(values):
    return dict(median_ns=statistics.median(values), min_ns=min(values), max_ns=max(values))


def verify():
    metadata = json.loads((OUT / 'metadata.json').read_text())
    assert metadata['clippy_exit'] == metadata['build_exit'] == metadata['run_exit'] == 0
    assert sha(OUT / 'probe') == metadata['binary_sha256']
    assert (OUT / 'probe').stat().st_size == metadata['binary_bytes']
    for name, expected in metadata['sources'].items():
        assert sha(OUT / 'source' / name) == expected
    assert metadata['scope'] == SCOPE and metadata['features'] == ['http-layout-probe']
    delta = [b - a for a, b in zip(metadata['cpu_before'], metadata['cpu_after'])]
    assert len(delta) >= 8 and all(v >= 0 for v in delta[:8]) and sum(delta[:8]) > 0
    blocks = [json.loads(line) for line in (OUT / 'blocks.jsonl').read_text().splitlines()]
    expected = [(r, SCENARIOS[(r + offset) % 4], b) for r in range(8) for offset in range(4) for b in range(20)]
    assert [(row['round'], row['scenario'], row['block']) for row in blocks] == expected
    for row in blocks:
        assert row['iterations'] == 20000 and row['elapsed_ns'] > 0
        phases = row['phases_active_ns_polls_pending']
        assert len(phases) == 10 and all(len(phase) == 3 for phase in phases)
        if row['scenario'] == 'phase-polls':
            assert all(ns > 0 and polls == 20000 and pending == 0 for ns, polls, pending in phases)
        else:
            assert phases == [[0, 0, 0] for _ in PHASES]
        assert (row['calibration_active_ns'] > 0) == (row['scenario'] == 'clock-calibration')
    scenarios = {}
    for scenario in SCENARIOS:
        rows = [row for row in blocks if row['scenario'] == scenario]
        values = [row['elapsed_ns'] / row['iterations'] for row in rows]
        scenarios[scenario] = dict(describe(values), round_medians_ns=[statistics.median(
            row['elapsed_ns'] / row['iterations'] for row in rows if row['round'] == r) for r in range(8)])
    phase_rows = [row for row in blocks if row['scenario'] == 'phase-polls']
    phases = {name: describe([row['phases_active_ns_polls_pending'][i][0] / row['iterations']
                              for row in phase_rows]) for i, name in enumerate(PHASES)}
    calibration = describe([row['calibration_active_ns'] / row['iterations'] for row in blocks
                            if row['scenario'] == 'clock-calibration'])
    summary = dict(source=metadata['source'], scope=SCOPE, raw_blocks=len(blocks),
        total_iterations=sum(row['iterations'] for row in blocks),
        host_steal_fraction=delta[7] / sum(delta[:8]), scenarios=scenarios,
        active_phase_poll_wall_ns=phases, clock_calibration_active_ns=calibration,
        pending_polls=0)
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
