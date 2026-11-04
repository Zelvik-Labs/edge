#!/usr/bin/env python3
"""Use the independent full wire fixture and observe origin connection reuse."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('bare_matrix', ROOT / 'scripts/perf/protocol_state_matrix.py')
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--variant', choices=['r0-w0', 'r64k-w0', 'r0-w1460', 'r64k-w1460'], required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--tls', type=Path, required=True)
parser.add_argument('--seed', type=int, required=True)
options = parser.parse_args()
observations = []
tls_controls = []


def origin_count(port):
    accepted, handled, requests = map(int, matrix.raw_curl(f'http://127.0.0.1:{port}/_stats').decode().splitlines()[2].split())
    observations.append(dict(port=port, accepted=accepted, handled=handled, requests=requests, observed_at=time.time()))
    matrix.write(options.output / 'origin-connections.json', observations)
    return requests


original_probe = matrix.probe
original_docker = matrix.docker
current_target = None


def docker(*command):
    global current_target
    if command[0] == 'run' and '--name' in command:
        name = command[command.index('--name') + 1]
        if name.endswith('-pingora'):
            current_target = name
    return original_docker(*command)


matrix.docker = docker


def probe(protocol, url, ca, payload, stem):
    result = original_probe(protocol, url, ca, payload, stem)
    if current_target:
        transcript = original_docker('logs', current_target)
        expected = f'bare-pingora buffers read={args.bare_read_buffer} write={args.bare_write_buffer}'
        assert expected in transcript
        actual = original_docker('exec', current_target, 'sha256sum', '/proc/1/exe').split()[0]
        assert actual == matrix.sha(args.bare_pingora_binary)
        matrix.write(options.output / 'buffer-identity.json', dict(read=args.bare_read_buffer,
            write=args.bare_write_buffer, executable_sha256=actual, startup_line=expected))
    if protocol in ['h1-tls', 'h2'] and protocol not in {r['protocol'] for r in tls_controls}:
        command = ['curl', '--noproxy', '*', '--silent', '--show-error', '--max-time', '20',
                   matrix.FLAGS[protocol], '--cacert', str(options.tls / 'wrong-ca.pem'), url]
        negative = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        tls_controls.append({'protocol': protocol, 'exit': negative.returncode, 'command': command,
                             'stdout_bytes': len(negative.stdout), 'stderr': negative.stderr.decode()})
        matrix.write(options.output / 'tls-controls.json', tls_controls)
        assert negative.returncode == 60, tls_controls[-1]
    return result


matrix.origin_count = origin_count
matrix.probe = probe
bins = ROOT / '.local/bare-pingora-binaries'
args = argparse.Namespace(target='pingora', output=options.output, tls=options.tls,
    client=bins / 'protocol-state-client', edge_binary=bins / 'Zelvik-Edge', controller=bins / 'issue34-controller',
    bare_pingora_binary=ROOT / '.local/pingora-buffers-ci/bare-pingora-proxy',
    bare_read_buffer=65536 if options.variant.startswith('r64k') else 0,
    bare_write_buffer=1460 if options.variant.endswith('w1460') else 0,
    client_source_revision='a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a',
    variant=options.variant,
    edge_image='edge-http-state-fixture:20261103', seconds=5, repeats=1, payload_size=1024, concurrency=32,
    edge_backend='pingora',
    capture_proxy_process_cpu=True,
    cold_requests=None, fresh_requests=2048, protocols=['h1', 'h1-tls', 'h2'], states=['off'],
    connections=['reuse'], multiplex=False, seed=options.seed,
    experiment='OFF-only framework control with immutable Edge; compare resource and wire conditions, not business feature equivalence')
raise SystemExit(matrix.run(args))
