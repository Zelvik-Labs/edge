#!/usr/bin/env python3
"""Four independent targets, balanced order, real protocols and timed misses."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / '.local/cache-range-private'
BINS = ROOT / '.local/cache-range-binaries'
ORDERS = [
    ['before', 'after', 'openresty', 'nginx'],
    ['after', 'nginx', 'before', 'openresty'],
    ['nginx', 'openresty', 'after', 'before'],
    ['openresty', 'before', 'nginx', 'after'],
]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def main():
    build = json.loads((BINS / 'build-metadata.json').read_text())
    assert build['builds_complete_before_measurement']
    matrix = module('cache_range_matrix', 'scripts/perf/protocol_state_matrix.py')
    for variant, data in build['variants'].items():
        for name, details in data['binaries'].items():
            assert matrix.sha(BINS / variant / name) == details['sha256']
    original_probe = matrix.probe
    controls = []
    tls_controls = []
    tls_seen = set()
    seen = set()

    def probe(protocol, url, ca, payload, stem):
        result = original_probe(protocol, url, ca, payload, stem)
        identity = (str(stem.parent), protocol)
        if protocol in ['h1-tls', 'h2', 'h3'] and identity not in tls_seen:
            tls_seen.add(identity)
            command = ['curl', '--ipv4', '--noproxy', '*', '--silent', '--show-error', '--max-time', '20',
                       matrix.FLAGS[protocol], '--cacert', str(OUT / 'tls/wrong-ca.pem'), url]
            negative = subprocess.run(command, capture_output=True, timeout=25)
            tls_controls.append(dict(directory=str(stem.parent.relative_to(OUT)), protocol=protocol,
                exit=negative.returncode, stdout_bytes=len(negative.stdout), stderr=negative.stderr.decode()))
            matrix.write(OUT / 'tls-controls.json', tls_controls)
            assert negative.returncode == 60, 'wrong CA must reject the actual protocol'
        if stem.name.endswith('-warm') and result['cache_status'] == 'HIT' and str(stem) not in seen:
            seen.add(str(stem))
            for index, (header, expected) in enumerate([('bytes=0-15', payload[:16]), ('bytes=-16', payload[-16:])]):
                body = Path(str(stem) + f'.range-{index}.body')
                headers = Path(str(stem) + f'.range-{index}.headers')
                command = ['curl', '--ipv4', '--noproxy', '*', '--silent', '--show-error', '--max-time', '20',
                           matrix.FLAGS[protocol], '--cacert', str(ca), '--header', 'Range: ' + header,
                           '--output', str(body), '--dump-header', str(headers), '--write-out',
                           '%{http_version}\t%{ssl_verify_result}\t%{http_code}', url]
                process = subprocess.run(command, capture_output=True, text=True, timeout=25)
                observation = {'case': str(stem.relative_to(OUT)), 'protocol': protocol, 'range': header,
                               'exit': process.returncode, 'output': process.stdout, 'stderr': process.stderr,
                               'body_sha256': matrix.sha(body) if body.exists() else None,
                               'expected_sha256': hashlib.sha256(expected).hexdigest()}
                controls.append(observation)
                matrix.write(OUT / 'range-controls.json', controls)
                if process.returncode or process.stdout != f'{matrix.VERSIONS[protocol]}\t0\t206' or body.read_bytes() != expected:
                    raise RuntimeError('real cached Range control failed; see retained response')
        return result

    matrix.probe = probe
    source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    suite = {'source': source, 'build': build, 'orders': ORDERS, 'runs': [],
             'scope': '1KiB, 32 concurrency, four protocols, three cache states and reuse/fresh modes; independent targets; no multiplex ranking',
             'client_sha256': matrix.sha(OUT / 'protocol-state-client'),
             'sources': {p: matrix.sha(ROOT / p) for p in ['scripts/perf/downstream-buffer/run.py',
                 'scripts/perf/protocol_state_matrix.py', 'scripts/perf/http-state/verify.py',
                 'benches/support/protocol_state_client.c']}}
    matrix.write(OUT / 'suite.json', suite)
    for repeat, order in enumerate(ORDERS):
        for target in order:
            variant = target if target in ['before', 'after'] else 'after'
            directory = OUT / f'round-{repeat}-{target}'
            args = argparse.Namespace(target='edge' if target in ['before', 'after'] else target,
                output=directory, tls=OUT / 'tls', client=OUT / 'protocol-state-client',
                edge_binary=BINS / variant / 'Zelvik-Edge', controller=BINS / variant / 'issue34-controller',
                edge_image='edge-http-state-fixture:20261103', seconds=5, repeats=1,
                payload_size=1024, concurrency=32, cold_requests=None, fresh_requests=2048,
                protocols=['h1', 'h1-tls', 'h2', 'h3'], states=['hit', 'off', 'miss'],
                connections=['reuse', 'fresh'], multiplex=False, seed=20261104,
                experiment=os.environ.get('EDGE_CACHE_COMPARISON_DESCRIPTION',
                    'read=0/write=1460 downstream buffering; all builds completed'),
                comparison_variant=target, binary_source=build['variants'][variant]['source'])
            failure = None
            try:
                code = matrix.run(args)
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                code = 1
                failure = str(error)
            suite['runs'].append({'round': repeat, 'variant': target,
                                 'manifest': str((directory / 'manifest.json').relative_to(OUT)),
                                 'exit': code, 'infrastructure_failure': failure})
            matrix.write(OUT / 'suite.json', suite)
            # Per-cell failures remain in the manifest and do not remove later targets.
    for variant, data in build['variants'].items():
        for name, details in data['binaries'].items():
            assert matrix.sha(BINS / variant / name) == details['sha256']
    if any(run['exit'] for run in suite['runs']):
        raise SystemExit('one or more cells failed; complete runs and failures retained')


if __name__ == '__main__':
    main()
