#!/usr/bin/env python3
"""Sequential protocol integrity gates and optional native capacity measurements.

Run inside the benchmark client node/pod. Remote port forwarding is unsuitable
for capacity comparisons and does not carry HTTP/3 UDP.
"""
import argparse
import hashlib
import ipaddress
import json
from pathlib import Path
import random
import subprocess
import tempfile
import time
from urllib.parse import urlsplit

PROTOCOLS = {"h1": ("1.1", "--http1.1", "http"),
             "h1-tls": ("1.1", "--http1.1", "https"),
             "h2": ("2", "--http2-prior-knowledge", "https"),
             "h3": ("3", "--http3-only", "https")}


def snapshot():
    path = Path('/proc/stat')
    return {"at": time.time(), "cpu_ticks": {
        line.split()[0]: list(map(int, line.split()[1:]))
        for line in path.read_text().splitlines() if line.startswith('cpu')
    } if path.exists() else None}


def connection(spec):
    url = urlsplit(spec['url'])
    if url.username or url.password or url.scheme not in {'http', 'https'}:
        raise ValueError('Use an HTTP(S) test URL without credentials')
    port = url.port or (443 if url.scheme == 'https' else 80)
    curl, oha = [], []
    if spec.get('ca'):
        if not Path(spec['ca']).is_file():
            raise ValueError('Configured CA file is missing')
        curl += ['--cacert', spec['ca']]
        oha += ['--cacert', spec['ca']]
    address = spec.get('connect_ip', url.hostname)
    if spec.get('connect_ip'):
        ipaddress.ip_address(address)
        curl += ['--resolve', f'{url.hostname}:{port}:{address}']
        oha += ['--connect-to', f'{url.hostname}:{port}:{address}:{port}']
    # A deliberately local fixture must not go through the managed egress proxy.
    if address == 'localhost' or address and _loopback(address):
        curl += ['--noproxy', '*']
    return url, curl, oha


def _loopback(address):
    try:
        return ipaddress.ip_address(address).is_loopback
    except ValueError:
        return False


def verify(protocol, spec, payload, curl_binary='curl'):
    version, flag, scheme = PROTOCOLS[protocol]
    url, options, _ = connection(spec)
    if url.scheme != scheme:
        raise ValueError(f'{protocol} requires {scheme}')
    with tempfile.TemporaryDirectory(prefix='edge-protocol-') as folder:
        body = Path(folder) / 'body'
        command = [curl_binary, '--silent', '--show-error', '--max-time', '20',
                   flag, *options, '--output', str(body), '--write-out',
                   '%{http_version}\t%{ssl_verify_result}\t%{http_code}', spec['url']]
        result = subprocess.run(command, text=True, capture_output=True, timeout=25)
        if result.returncode:
            raise RuntimeError(f'curl exit {result.returncode}: {result.stderr.strip()}')
        negotiated, verified, code = result.stdout.strip().split('\t')
        if (negotiated, verified, code) != (version, '0', '200'):
            raise RuntimeError(f'protocol/TLS/status mismatch: {result.stdout.strip()}')
        received = body.read_bytes()
        if received != payload:
            raise RuntimeError('response body mismatch')
        return {'negotiated': negotiated, 'tls_verify_result': int(verified),
                'status_code': int(code), 'bytes': len(received),
                'sha256': hashlib.sha256(received).hexdigest()}


def native(args, protocol, spec, output):
    if Path('/proc').exists():
        for path in Path('/proc').glob('[0-9]*/comm'):
            try:
                name = path.read_text().strip()
            except FileNotFoundError:
                continue
            if name in {'rustc', 'mbx-rustc'}:
                try:
                    status = path.with_name('status').read_text()
                except FileNotFoundError:
                    continue
                if any(line.startswith('State:') and line.split()[1] == 'Z'
                       for line in status.splitlines()):
                    continue
                raise RuntimeError('Capacity measurement cannot overlap compilation')
    _, _, options = connection(spec)
    command = [args.oha, '--no-tui', '--output-format', 'json', '--http-version',
               PROTOCOLS[protocol][0], '--disable-compression', '--worker-threads', '1',
               '-z', f'{args.seconds}s', '-w', '-c', str(args.concurrency), '-t', '10s',
               *options, spec['url']]
    if args.client_cpu is not None:
        command = ['taskset', '-c', str(args.client_cpu), *command]
    if protocol == 'h2':
        command += ['-p', '1']
    output.with_suffix('.command.json').write_text(json.dumps(command, indent=2))
    stderr_path = output.with_suffix('.stderr.log')
    with output.open('x') as handle, stderr_path.open('x') as stderr:
        completed = subprocess.run(command, stdout=handle, stderr=stderr,
                                   timeout=args.seconds + 35)
    if completed.returncode:
        raise RuntimeError(f'native client exit {completed.returncode}: '
                           f'{stderr_path.read_text()[-2000:]}')
    result = json.loads(output.read_text())
    if result['errorDistribution'] or set(result['statusCodeDistribution']) != {'200'}:
        raise RuntimeError('native client reported errors or non-200 responses')
    return result


def run(args):
    config = json.loads(args.config.read_text())
    if args.oha and len(config['targets']) != 1:
        raise ValueError('Native independent runs require exactly one target')
    payload = Path(config['payload']).read_bytes()
    if not payload:
        raise ValueError('Empty payload is not a valid integrity fixture')
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = {'started_at': time.time(), 'mode': 'native' if args.oha else 'integrity-only',
                'seed': args.seed, 'concurrency': args.concurrency, 'seconds': args.seconds,
                'http2_parallel': 1,
                'payload_sha256': hashlib.sha256(payload).hexdigest(),
                'driver_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                'config_sha256': hashlib.sha256(args.config.read_bytes()).hexdigest(),
                'curl_version': subprocess.check_output([args.curl, '--version'], text=True),
                'cases': [], 'completed': False}
    if args.oha:
        manifest['oha_version'] = subprocess.check_output([args.oha, '--version'], text=True).strip()
        manifest['oha_sha256'] = hashlib.sha256(Path(args.oha).read_bytes()).hexdigest()
    rng = random.Random(args.seed)
    failed = False
    try:
        for repeat in range(args.repeats):
            cases = [(name, protocol, spec) for name, target in config['targets'].items()
                     for protocol, spec in target.items()]
            rng.shuffle(cases)
            for name, protocol, spec in cases:
                if protocol not in PROTOCOLS:
                    raise ValueError(f'Unknown protocol: {protocol}')
                row = {'target': name, 'protocol': protocol, 'repeat': repeat,
                       'before': snapshot()}
                manifest['cases'].append(row)
                try:
                    if 'unsupported' in spec:
                        if not spec['unsupported'] or 'url' in spec:
                            raise ValueError('Unsupported needs an evidence reason and no URL')
                        row.update(status='not-supported', reason=spec['unsupported'])
                        continue
                    row['integrity'] = verify(protocol, spec, payload, args.curl)
                    if args.oha:
                        filename = f'{len(manifest["cases"]):04d}.oha.json'
                        result = native(args, protocol, spec, args.output / filename)
                        if result['summary']['sizePerRequest'] != len(payload):
                            raise RuntimeError('native response size differs from integrity fixture')
                        row.update(native_file=filename, rps=result['summary']['requestsPerSec'])
                    row['status'] = 'passed'
                except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
                    row.update(status='failed', error=str(exc))
                    failed = True
                finally:
                    row['after'] = snapshot()
                    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2))
        manifest['completed'] = True
    finally:
        manifest['ended_at'] = time.time()
        (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2))
    return 1 if failed else 0


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--curl', default='curl')
    parser.add_argument('--oha', help='absolute native binary path; omitted means integrity only')
    parser.add_argument('--seconds', type=int, default=30)
    parser.add_argument('--concurrency', type=int, default=32)
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--seed', type=int, default=20261102)
    parser.add_argument('--client-cpu', type=int)
    options = parser.parse_args()
    if min(options.seconds, options.concurrency, options.repeats) < 1:
        parser.error('seconds, concurrency and repeats must be positive')
    raise SystemExit(run(options))
