#!/usr/bin/env python3
"""Diagnose native/shared QUIC fresh connections; all timing is instrumented."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/quinn-diagnostic-private'
BINS = ROOT / '.local/quinn-diagnostic-binaries'


def main():
    spec = importlib.util.spec_from_file_location('diagnostic_matrix', ROOT / 'scripts/perf/protocol_state_matrix.py')
    matrix = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(matrix)
    native_docker = matrix.docker

    def docker(*args):
        args = list(args)
        if args[0] == 'run' and 'RUST_LOG=warn' in args:
            args[args.index('RUST_LOG=warn')] = 'RUST_LOG=warn,zelvik_edge::http3_proxy_manager=info'
        return native_docker(*args)

    matrix.docker = docker
    build = json.loads((OUT / 'build-metadata.json').read_text())
    for name, digest in build['binaries'].items():
        assert matrix.sha(BINS / name) == digest
    suite = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'build': build, 'client_sha256': matrix.sha(OUT / 'protocol-state-client'), 'runs': [],
        'scope': 'instrumented protocol diagnosis only; no capacity or latency comparison',
        'trace_bound_per_active_slot_bytes': 65536, 'native_fresh_requests_per_case': 16384,
        'shared_fresh_requests_per_case': 64,
        'sources': {name: matrix.sha(ROOT / name) for name in ['benches/support/protocol_state_client.c',
            'benches/support/quic_client_diagnostics.h', 'scripts/perf/protocol_state_matrix.py',
            'scripts/perf/quinn-diagnostic/run.py']}}
    matrix.write(OUT / 'suite.json', suite)
    for repeat, order in enumerate([['quinn-native', 'quinn-shared'], ['quinn-shared', 'quinn-native']]):
        for backend in order:
            directory = OUT / f'round-{repeat}-{backend}'
            limit = 16384 if backend == 'quinn-native' else 64
            args = argparse.Namespace(target='edge', output=directory, tls=OUT / 'tls',
                client=OUT / 'protocol-state-client', edge_binary=BINS / 'Zelvik-Edge',
                controller=BINS / 'issue34-controller', edge_image='edge-http-state-fixture:20261103',
                edge_backend=backend, transport_profile='matched', address_validation='off',
                quinn_native_gso='auto', seconds=5, repeats=1, payload_size=32768, concurrency=32,
                cold_requests=None, fresh_requests=limit, protocols=['h3'], states=['hit'],
                connections=['fresh'], multiplex=False, seed=20261104,
                experiment='bounded OpenSSL frame diagnosis; instrumented traffic excluded from capacity ranking')
            error = None
            try:
                code = matrix.run(args)
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exception:
                code, error = 1, str(exception)
            native_lines = []
            for log in sorted(directory.glob('edge*.log')):
                native_lines.extend(re.sub(r'\x1b\[[0-9;]*m', '', line) for line in log.read_text(errors='replace').splitlines()
                    if 'HTTP/3 experimental native Quinn listener on' in line)
            suite['runs'].append({'round': repeat, 'backend': backend, 'exit': code, 'infrastructure_error': error,
                'request_limit': limit,
                'native_listener_startup': native_lines,
                'manifest': str((directory / 'manifest.json').relative_to(OUT))})
            matrix.write(OUT / 'suite.json', suite)
    for name, digest in build['binaries'].items():
        assert matrix.sha(BINS / name) == digest
    if any(run['exit'] for run in suite['runs']):
        raise SystemExit('diagnostic traffic contains failures; retained for protocol cause analysis')


if __name__ == '__main__':
    main()
