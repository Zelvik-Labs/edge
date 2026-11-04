"""Inspect negotiated TLS algorithms separately from all capacity measurements."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess

root = Path.cwd()
spec = importlib.util.spec_from_file_location('matrix', root / 'scripts/perf/protocol_state_matrix.py')
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)
original_probe = matrix.probe
def probe(protocol, url, ca, payload, stem):
    result = original_probe(protocol, url, ca, payload, stem)
    command = ['curl', '--silent', '--show-error', '--verbose', '--http1.1', '--no-sessionid',
               '--noproxy', '*', '--cacert', str(ca), '--max-time', '15',
               '--output', '/dev/null', '--write-out', '%{http_code} %{http_version} %{ssl_verify_result}', url]
    observed = subprocess.run(command, env=matrix.ENV, capture_output=True, text=True)
    record = {'command': command, 'returncode': observed.returncode, 'result': observed.stdout,
              'tls_lines': [line for line in observed.stderr.splitlines() if any(
                  value in line for value in ['SSL connection using', 'ALPN: server accepted',
                                              'SSL certificate verify ok', 'Server certificate:',
                                              'Certificate level', 'subject:'])]}
    Path(str(stem) + '.tls-negotiation.json').write_text(json.dumps(record, indent=2) + '\n')
    Path(str(stem) + '.tls-negotiation.stderr').write_text(observed.stderr)
    if observed.returncode or observed.stdout.strip() != '200 1.1 0':
        raise RuntimeError('TLS negotiation diagnostic failed; inspect saved evidence')
    return result
matrix.probe = probe
for target in ['edge', 'nginx', 'openresty']:
    args = argparse.Namespace(target=target, output=root / '.local/architecture-stage2' / ('tls-negotiation-' + target),
        tls=root / '.local/perf-next/tls', client=root / '.local/protocol-state-matrix/protocol-state-client',
        edge_binary=Path('/workspace/edge/target/release/Zelvik-Edge'),
        controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
        seconds=1, repeats=1, payload_size=1024, concurrency=32, cold_requests=128, fresh_requests=128,
        protocols=['h1-tls'], states=['off'], connections=['reuse'], multiplex=False, seed=20261102,
        idle_reference_seconds=0, experiment='TLS algorithms diagnostic only; exclude from capacity',
        diagnostic_driver_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    code = matrix.run(args)
    if code:
        raise RuntimeError('TLS diagnostic fixture failed; inspect before proceeding')
