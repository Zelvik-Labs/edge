#!/usr/bin/env python3
"""Same client source and Debian curl version, two independent QUIC backends."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/quinn-interop-private'
BINS = ROOT / '.local/quinn-diagnostic-binaries'


def main():
    spec = importlib.util.spec_from_file_location('interop_matrix', ROOT / 'scripts/perf/protocol_state_matrix.py')
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
    clients = {}
    for variant in ['openssl', 'gnutls']:
        client = OUT / 'clients' / variant
        clients[variant] = {'sha256': matrix.sha(client),
            'version': subprocess.check_output([str(client), '--version'], text=True).strip()}
    assert 'OpenSSL/' in clients['openssl']['version']
    assert 'GnuTLS/' in clients['gnutls']['version'] and 'ngtcp2/' in clients['gnutls']['version']
    suite = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'build': build, 'clients': clients, 'runs': [], 'request_limit': 16384,
        'client_package_version': json.loads((OUT / 'client-build.json').read_text())['package_version'],
        'scope': 'client interoperability and errors; different client stacks are not a server capacity comparison',
        'client_source_sha256': matrix.sha(ROOT / 'benches/support/protocol_state_client.c')}
    matrix.write(OUT / 'suite.json', suite)
    for repeat, order in enumerate([['openssl', 'gnutls'], ['gnutls', 'openssl']]):
        for variant in order:
            directory = OUT / f'round-{repeat}-{variant}'
            args = argparse.Namespace(target='edge', output=directory, tls=OUT / 'tls',
                client=OUT / 'clients' / variant, edge_binary=BINS / 'Zelvik-Edge',
                controller=BINS / 'issue34-controller', edge_image='edge-http-state-fixture:20261103',
                edge_backend='quinn-native', transport_profile='matched', address_validation='off',
                quinn_native_gso='auto', seconds=5, repeats=1, payload_size=32768, concurrency=32,
                cold_requests=None, fresh_requests=16384, protocols=['h3'], states=['hit'],
                connections=['fresh'], multiplex=False, seed=20261104,
                experiment='same client source/curl package, OpenSSL QUIC versus GnuTLS/ngtcp2; no capacity ranking')
            error = None
            try:
                code = matrix.run(args)
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exception:
                code, error = 1, str(exception)
            lines = []
            for log in sorted(directory.glob('edge*.log')):
                lines.extend(re.sub(r'\x1b\[[0-9;]*m', '', line) for line in log.read_text(errors='replace').splitlines()
                    if 'HTTP/3 experimental native Quinn listener on' in line)
            suite['runs'].append({'round': repeat, 'client': variant, 'exit': code, 'infrastructure_error': error,
                'native_listener_startup': lines, 'manifest': str((directory / 'manifest.json').relative_to(OUT))})
            matrix.write(OUT / 'suite.json', suite)
    for name, digest in build['binaries'].items():
        assert matrix.sha(BINS / name) == digest
    if any(run['exit'] for run in suite['runs']):
        raise SystemExit('interoperability traffic contains failures; all raw outcomes retained')


if __name__ == '__main__':
    main()
