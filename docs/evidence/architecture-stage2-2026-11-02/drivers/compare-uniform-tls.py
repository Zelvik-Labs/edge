"""Fresh TLS comparison with identical client protocol, cipher and group offers."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import time

root = Path.cwd()
local = root / '.local/architecture-stage2'
config = local / 'openssl-x25519.cnf'
os.environ['OPENSSL_CONF'] = str(config)
spec = importlib.util.spec_from_file_location('matrix', root / 'scripts/perf/protocol_state_matrix.py')
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)
original_probe = matrix.probe
def probe(protocol, url, ca, payload, stem):
    result = original_probe(protocol, url, ca, payload, stem)
    if protocol not in {'h1-tls', 'h2'}:
        return result
    command = ['curl', '--silent', '--show-error', '--verbose',
               '--http1.1' if protocol == 'h1-tls' else '--http2', '--no-sessionid',
               '--noproxy', '*', '--cacert', str(ca), '--max-time', '15', '--output', '/dev/null',
               '--write-out', '%{http_code} %{http_version} %{ssl_verify_result}', url]
    observed = subprocess.run(command, env=matrix.ENV, capture_output=True, text=True)
    lines = [line for line in observed.stderr.splitlines() if any(value in line for value in [
        'SSL connection using', 'ALPN: server accepted', 'SSL certificate verify ok.'])]
    record = {'command': command, 'returncode': observed.returncode, 'result': observed.stdout,
              'tls_lines': lines, 'OPENSSL_CONF': str(config),
              'openssl_conf_sha256': hashlib.sha256(config.read_bytes()).hexdigest()}
    Path(str(stem) + '.tls-negotiation.json').write_text(json.dumps(record, indent=2) + '\n')
    Path(str(stem) + '.tls-negotiation.stderr').write_text(observed.stderr)
    expected = '200 1.1 0' if protocol == 'h1-tls' else '200 2 0'
    if observed.returncode or observed.stdout.strip() != expected or not any(
        'TLSv1.3 / TLS_AES_256_GCM_SHA384 / X25519 / RSASSA-PSS'.lower() in line.lower()
        for line in lines):
        raise RuntimeError('TLS protocol/cipher/group/verification does not match the controlled profile')
    return result
matrix.probe = probe
output = local / 'comparison-uniform-tls'
output.mkdir(exist_ok=False)
index = {'status': 'running', 'started_at': time.time(), 'runs': [],
         'client_tls_profile': 'TLSv1.3 / TLS_AES_256_GCM_SHA384 / X25519; RSA-2048 fixture',
         'openssl_conf_sha256': hashlib.sha256(config.read_bytes()).hexdigest()}
def save():
    (output / 'index.json').write_text(json.dumps(index, indent=2) + '\n')
save()
for label in ['baseline', 'candidate', 'nginx', 'openresty']:
    destination = output / ('00-' + label)
    binary = local / 'baseline-Zelvik-Edge' if label == 'baseline' else Path('/workspace/edge/target/release/Zelvik-Edge')
    args = argparse.Namespace(target='edge' if label in {'baseline', 'candidate'} else label,
        output=destination, tls=root / '.local/perf-next/tls',
        client=root / '.local/protocol-state-matrix/protocol-state-client', edge_binary=binary,
        controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
        seconds=8, repeats=1, payload_size=1024, concurrency=32, cold_requests=4096, fresh_requests=2048,
        protocols=['h1-tls', 'h2'], states=['off', 'hit', 'miss'], connections=['fresh'],
        multiplex=False, seed=20261102, idle_reference_seconds=2,
        client_tls_profile=index['client_tls_profile'], client_OPENSSL_CONF=str(config),
        client_openssl_conf_sha256=index['openssl_conf_sha256'],
        comparison_driver_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    row = {'label': label, 'repeat': 0, 'manifest': str(destination / 'manifest.json')}
    index['runs'].append(row)
    save()
    row['returncode'] = matrix.run(args)
    manifest = json.loads((destination / 'manifest.json').read_text())
    row['status'] = manifest['status']
    row['case_statuses'] = [c['status'] for c in manifest['cases']]
    save()
    if manifest['status'] != 'completed' or manifest.get('cleanup_errors') or row['returncode']:
        raise RuntimeError('Controlled TLS fixture failed; retain result and inspect')
index.update(status='completed', ended_at=time.time(), all_cases_passed=True)
save()
