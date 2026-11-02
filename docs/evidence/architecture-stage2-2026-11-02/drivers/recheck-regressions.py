"""Two extra interleaved rounds to investigate large H2 and fresh-H1 regressions."""
import json
from pathlib import Path
import subprocess
import sys
import time

root = Path.cwd()
local = root / '.local/architecture-stage2'
output = local / 'regression-recheck'
output.mkdir(exist_ok=False)
index = {'status': 'running', 'started_at': time.time(), 'runs': [],
         'reason': 'Initial candidate H2 multiplex/fresh MISS and H1 fresh OFF CPU/response increased sharply; retain all initial samples and test reproducibility.'}
def save():
    (output / 'index.json').write_text(json.dumps(index, indent=2) + '\n')
save()
for repeat in [1, 2]:
    labels = ['candidate', 'baseline'] if repeat == 1 else ['baseline', 'candidate']
    for label in labels:
        for kind, protocol, mode in [('h2-reuse', 'h2', ['--connections', 'reuse', '--multiplex']),
                           ('h2-fresh-miss', 'h2', ['--connections', 'fresh', '--states', 'miss', '--fresh-requests', '2048']),
                           ('h1-fresh-off', 'h1', ['--connections', 'fresh', '--states', 'off', '--fresh-requests', '2048'])]:
            destination = output / f'{repeat:02d}-{label}-{kind}'
            binary = local / 'baseline-Zelvik-Edge' if label == 'baseline' else Path('/workspace/edge/target/release/Zelvik-Edge')
            command = [sys.executable, 'scripts/perf/protocol_state_matrix.py', '--target', 'edge',
                       '--edge-binary', str(binary), '--output', str(destination),
                       '--tls', str(root / '.local/perf-next/tls'),
                       '--client', str(root / '.local/protocol-state-matrix/protocol-state-client'),
                       '--protocols', protocol, '--repeats', '1', *mode,
                       '--seconds', '8', '--cold-requests', '4096', '--idle-reference-seconds', '2']
            row = {'label': label, 'repeat': repeat, 'command': command, 'kind': kind,
                   'manifest': str(destination / 'manifest.json')}
            index['runs'].append(row)
            save()
            with (output / f'{repeat:02d}-{label}-{kind}.log').open('x') as file:
                row['returncode'] = subprocess.run(command, stdout=file, stderr=subprocess.STDOUT).returncode
            manifest = json.loads((destination / 'manifest.json').read_text())
            row['status'] = manifest['status']
            row['case_statuses'] = [c['status'] for c in manifest['cases']]
            save()
            print(json.dumps(row), flush=True)
            if manifest['status'] != 'completed' or manifest.get('cleanup_errors'):
                raise RuntimeError('Incomplete fixture/cleanup; stop before next run')
index.update(status='completed', ended_at=time.time(),
             all_cases_passed=all(r['returncode'] == 0 for r in index['runs']))
save()
raise SystemExit(int(not index['all_cases_passed']))
