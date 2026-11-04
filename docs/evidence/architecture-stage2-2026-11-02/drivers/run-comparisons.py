"""Sequential capacity runs; every failure and process exit is retained."""
import json
from pathlib import Path
import subprocess
import sys

root = Path.cwd()
local = root / '.local/architecture-stage2'
common = ['--tls', str(root / '.local/perf-next/tls'),
          '--client', str(root / '.local/protocol-state-matrix/protocol-state-client')]
compare = [sys.executable, 'scripts/perf/compare_architecture_candidates.py',
           '--baseline', str(local / 'baseline-Zelvik-Edge'),
           '--candidate', '/workspace/edge/target/release/Zelvik-Edge', *common]
cases = [
    ('comparison-h1', ['--protocols', 'h1', '--repeats', '3']),
    ('comparison-tls-h2', ['--protocols', 'h1-tls', 'h2', '--multiplex', '--repeats', '1']),
    ('comparison-fresh', ['--protocols', 'h1', 'h1-tls', 'h2', '--connections', 'fresh',
                          '--fresh-requests', '2048', '--repeats', '1']),
    ('comparison-cold-long', ['--protocols', 'h1', '--states', 'miss', '--cold-requests',
                             '32768', '--repeats', '3']),
    ('comparison-32k', ['--protocols', 'h1', '--payload-size', '32768', '--cold-requests',
                       '512', '--seconds', '5', '--repeats', '1']),
]
results = []
for name, options in cases:
    command = compare + ['--output', str(local / name), *options]
    with (local / (name + '.log')).open('x') as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    index_path = local / name / 'index.json'
    index = json.loads(index_path.read_text()) if index_path.exists() else {}
    row = {'name': name, 'command': command, 'returncode': result.returncode,
           'status': index.get('status'), 'all_cases_passed': index.get('all_cases_passed')}
    results.append(row)
    (local / 'comparison-runs.json').write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps(row), flush=True)
    if row['status'] != 'completed':
        raise RuntimeError('Fixture or cleanup did not complete; stop before the next run')
raise SystemExit(int(any(row['returncode'] for row in results)))
