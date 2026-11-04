#!/usr/bin/env python3
"""Isolate native Quinn transmit GSO on the two earlier fresh-connection failures."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/quinn-gso-private'
BINS = ROOT / '.local/quinn-gso-binaries'


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def main():
    matrix = module('quinn_gso_matrix', 'scripts/perf/protocol_state_matrix.py')
    native_docker = matrix.docker

    def docker(*args):
        args = list(args)
        # This module logs listener lifecycle, not every packet/request. Keep
        # the rest of the node at WARN; record the effective startup mode.
        if args[0] == 'run' and 'RUST_LOG=warn' in args:
            args[args.index('RUST_LOG=warn')] = 'RUST_LOG=warn,zelvik_edge::http3_proxy_manager=info'
        return native_docker(*args)

    matrix.docker = docker
    source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    binary_hashes = {p.name: matrix.sha(p) for p in BINS.iterdir() if p.is_file()}
    suite = {'source': source, 'binary_sha256': binary_hashes,
             'client_sha256': matrix.sha(OUT / 'protocol-state-client'),
             'profile': 'matched; native Quinn; Retry off; RX unchanged; only TX GSO auto/off differs',
             'fresh_requests_per_case': 16384, 'runs': [],
             'scope': 'two historical failure cells plus reuse controls; no complete H3 or long-soak claim',
             'source_sha256': {p: matrix.sha(ROOT / p) for p in [
                 'src/h3_backend.rs', 'src/http3_proxy_manager.rs', 'Cargo.lock',
                 'scripts/perf/quinn-gso/run.py', 'scripts/perf/protocol_state_matrix.py',
                 'benches/support/protocol_state_client.c']}}
    matrix.write(OUT / 'suite.json', suite)
    for repeat in range(4):
        for size, state in [(32768, 'hit'), (131072, 'off')]:
            for mode in (['auto', 'off'] if repeat % 2 == 0 else ['off', 'auto']):
                directory = OUT / f'round-{repeat}-{mode}-{size}-{state}'
                args = argparse.Namespace(target='edge', output=directory, tls=OUT / 'tls',
                    client=OUT / 'protocol-state-client', edge_binary=BINS / 'Zelvik-Edge',
                    controller=BINS / 'issue34-controller', edge_image='edge-http-state-fixture:20261103',
                    edge_backend='quinn-native', transport_profile='matched', address_validation='off',
                    quinn_native_gso=mode, seconds=5, repeats=1, payload_size=size, concurrency=32,
                    cold_requests=None, fresh_requests=16384, protocols=['h3'], states=[state],
                    connections=['fresh', 'reuse'], multiplex=False, seed=20261104,
                    experiment='same-binary native Quinn GSO ablation; prior rare errors remain unresolved')
                code = matrix.run(args)
                lines = []
                for log in sorted(directory.glob('edge*.log')):
                    for line in log.read_text(errors='replace').splitlines():
                        if 'HTTP/3 experimental native Quinn listener on' in line:
                            lines.append(re.sub(r'\x1b\[[0-9;]*m', '', line))
                expected = 'gso=' + ('Auto' if mode == 'auto' else 'Off')
                assert len(lines) >= 2 and all(expected in line for line in lines), lines
                suite['runs'].append({'round': repeat, 'mode': mode, 'bytes': size, 'cache': state,
                    'manifest': str((directory / 'manifest.json').relative_to(OUT)), 'exit': code,
                    'native_listener_startup': lines})
                matrix.write(OUT / 'suite.json', suite)
    for name, sha in binary_hashes.items():
        assert matrix.sha(BINS / name) == sha
    if any(run['exit'] for run in suite['runs']):
        raise SystemExit('native GSO ablation includes failures; all completed cells retained')


if __name__ == '__main__':
    main()
