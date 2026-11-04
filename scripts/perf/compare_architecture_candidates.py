#!/usr/bin/env python3
"""Interleave independent production baseline/candidate and packaged competitors.

Every protocol/state/repeat case still restarts the target and tmpfs. The index
retains nonzero exit codes and every case failure, including the old H2 policy.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import time


def main(args):
    root = Path(__file__).resolve().parents[2]
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    index = {'started_at': time.time(), 'runs': [], 'status': 'running',
             'interleaved_labels': ['baseline', 'candidate', 'nginx', 'openresty']}
    def save():
        (out / 'index.json').write_text(json.dumps(index, indent=2) + '\n')
    save()
    for repeat in range(args.repeats):
        # Reverse adjacent pairs between rounds instead of always giving one
        # version the same place in a cloud host's load/temperature trajectory.
        labels = ['baseline', 'candidate', 'nginx', 'openresty'] if repeat % 2 == 0 else [
            'candidate', 'baseline', 'openresty', 'nginx']
        for label in labels:
            directory = out / f'{repeat:02d}-{label}'
            target = 'edge' if label in {'baseline', 'candidate'} else label
            command = [sys.executable, str(root / 'scripts/perf/protocol_state_matrix.py'),
                '--target', target, '--output', str(directory), '--tls', str(args.tls.resolve()),
                '--client', str(args.client.resolve()), '--controller', str(args.controller.resolve()),
                '--edge-binary', str((args.baseline if label == 'baseline' else args.candidate).resolve()),
                '--seconds', str(args.seconds), '--repeats', '1', '--concurrency', '32',
                '--idle-reference-seconds', str(args.idle_reference_seconds),
                '--payload-size', str(args.payload_size), '--cold-requests', str(args.cold_requests),
                '--fresh-requests', str(args.fresh_requests),
                '--connections', *args.connections, '--protocols', *args.protocols, '--states', *args.states]
            if args.multiplex:
                command += ['--multiplex']
            entry = {'label': label, 'repeat': repeat, 'command': command,
                     'manifest': str(directory / 'manifest.json')}
            index['runs'].append(entry)
            save()
            with (out / f'{repeat:02d}-{label}.log').open('x') as log:
                entry['returncode'] = subprocess.run(command, cwd=root, stdout=log,
                                                     stderr=subprocess.STDOUT).returncode
            if not (directory / 'manifest.json').exists():
                raise RuntimeError('fixture did not produce a manifest: ' + label)
            manifest = json.loads((directory / 'manifest.json').read_text())
            entry['status'] = manifest['status']
            entry['case_statuses'] = [case.get('status') for case in manifest['cases']]
            save()
            print(json.dumps(entry, ensure_ascii=False), flush=True)
            if manifest['status'] != 'completed' or manifest.get('cleanup_errors'):
                raise RuntimeError('incomplete fixture/cleanup: ' + label)
    index['status'] = 'completed'
    index['ended_at'] = time.time()
    index['all_cases_passed'] = all(r['returncode'] == 0 for r in index['runs'])
    save()
    return int(not index['all_cases_passed'])


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tls', type=Path, required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--controller', type=Path, default=Path('/workspace/edge/target/release/examples/issue34-controller'))
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--seconds', type=int, default=8)
    parser.add_argument('--payload-size', type=int, default=1024)
    parser.add_argument('--cold-requests', type=int, default=4096)
    parser.add_argument('--fresh-requests', type=int, default=2048)
    parser.add_argument('--connections', nargs='+', choices=['reuse', 'fresh'], default=['reuse'])
    parser.add_argument('--protocols', nargs='+', choices=['h1', 'h1-tls', 'h2', 'h3'], default=['h1'])
    parser.add_argument('--states', nargs='+', choices=['off', 'hit', 'miss'], default=['off', 'hit', 'miss'])
    parser.add_argument('--multiplex', action='store_true')
    parser.add_argument('--idle-reference-seconds', type=float, default=2)
    raise SystemExit(main(parser.parse_args()))
