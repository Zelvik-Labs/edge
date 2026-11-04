#!/usr/bin/env python3
"""Same client source and Debian curl version, two independent QUIC backends."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/quinn-interop-private'
BINS = ROOT / '.local/quinn-diagnostic-binaries'


def main():
    mode = os.environ.get('EDGE_QUIC_INTEROP_MODE', 'tls-backend')
    if mode == 'tls-backend':
        plans = {'openssl': {'binary': 'openssl', 'curves': None}, 'gnutls': {'binary': 'gnutls', 'curves': None}}
    elif mode == 'groups':
        plans = {'default': {'binary': 'openssl', 'curves': None}, 'x25519': {'binary': 'openssl', 'curves': 'X25519'}}
    else:
        raise ValueError('unknown interoperability mode: ' + mode)
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
    original_probe = matrix.probe
    tls_controls = []

    def probe(protocol, url, ca, payload, stem):
        result = original_probe(protocol, url, ca, payload, stem)
        if protocol == 'h3' and stem.name.endswith('-before'):
            variant = stem.parent.name.rsplit('-', 1)[-1]
            command = [str(OUT / 'clients' / plans[variant]['binary']), '--protocol', 'h3', '--url', url,
                '--cacert', str(OUT / 'tls/wrong-ca.pem'), '--payload', str(stem.parent / 'payload'),
                '--connections', '1', '--requests', '1', '--seconds', '5', '--fresh', '--fixture-ca-only']
            if plans[variant]['curves']:
                command += ['--curves', plans[variant]['curves']]
            process = subprocess.run(command, capture_output=True, text=True, timeout=20)
            fields = process.stdout.strip().split('\t')
            observation = {'case': stem.parent.name, 'client': variant, 'command': command,
                'exit': process.returncode, 'stdout': process.stdout, 'stderr': process.stderr}
            tls_controls.append(observation)
            matrix.write(OUT / 'tls-controls.json', tls_controls)
            if process.returncode or len(fields) != 8 or fields[0] != '0' or fields[6] != '60':
                raise RuntimeError('wrong-CA control did not fail certificate verification; retained tls-controls.json')
        return result

    matrix.probe = probe
    build = json.loads((OUT / 'build-metadata.json').read_text())
    for name, digest in build['binaries'].items():
        assert matrix.sha(BINS / name) == digest
    clients = {}
    for variant, plan in plans.items():
        client = OUT / 'clients' / plan['binary']
        clients[variant] = {**plan, 'sha256': matrix.sha(client),
            'version': subprocess.check_output([str(client), '--version'], text=True).strip()}
    if mode == 'tls-backend':
        assert 'OpenSSL/' in clients['openssl']['version']
        assert 'GnuTLS/' in clients['gnutls']['version'] and 'ngtcp2/' in clients['gnutls']['version']
    else:
        assert clients['default']['sha256'] == clients['x25519']['sha256']
        assert all('OpenSSL/' in row['version'] for row in clients.values())
    suite = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'build': build, 'clients': clients, 'runs': [], 'request_limit': 16384, 'client_mode': mode,
        'client_package_version': json.loads((OUT / 'client-build.json').read_text())['package_version'],
        'client_fixture_ca_only': True,
        'scope': 'client interoperability/group diagnosis; no server capacity comparison; group mode is instrumented',
        'client_source_sha256': matrix.sha(ROOT / 'benches/support/protocol_state_client.c')}
    matrix.write(OUT / 'suite.json', suite)
    labels = list(plans)
    for repeat, order in enumerate([labels, list(reversed(labels))]):
        for variant in order:
            directory = OUT / f'round-{repeat}-{variant}'
            args = argparse.Namespace(target='edge', output=directory, tls=OUT / 'tls',
                client=OUT / 'clients' / plans[variant]['binary'], edge_binary=BINS / 'Zelvik-Edge',
                controller=BINS / 'issue34-controller', edge_image='edge-http-state-fixture:20261103',
                edge_backend='quinn-native', transport_profile='matched', address_validation='off',
                quinn_native_gso='auto', seconds=5, repeats=1, payload_size=32768, concurrency=32,
                cold_requests=None, fresh_requests=16384, protocols=['h3'], states=['hit'],
                connections=['fresh'], multiplex=False, seed=20261104,
                fixture_ca_only=True,
                client_curves=plans[variant]['curves'],
                experiment='same server; client-mode=' + mode + '; compatibility only, no capacity ranking')
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
