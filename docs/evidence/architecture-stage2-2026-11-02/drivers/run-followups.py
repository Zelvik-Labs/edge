"""Functional checks and profiling, after all independent capacity runs finish."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

local = Path('.local/architecture-stage2')
common = ['--tls', '.local/perf-next/tls', '--client',
          '.local/protocol-state-matrix/protocol-state-client',
          '--edge-binary', '/workspace/edge/target/release/Zelvik-Edge']
jobs = [
    ('functional-128k', [sys.executable, 'scripts/perf/protocol_state_matrix.py',
                        '--target', 'edge', '--protocols', 'h1', '--states', 'hit', 'miss',
                        '--connections', 'reuse', '--payload-size', '131072',
                        '--cold-requests', '128', '--seconds', '3', '--repeats', '1', *common]),
    ('functional-h3', [sys.executable, 'scripts/perf/protocol_state_matrix.py',
                      '--target', 'edge', '--protocols', 'h3', '--states', 'off',
                      '--connections', 'reuse', '--seconds', '3', '--repeats', '1', *common]),
    ('profiles-after', [sys.executable, 'scripts/perf/profile_architecture.py',
                        '--seconds', '15', '--cold-requests', '32768', *common]),
]
records = []
for name, command in jobs:
    command += ['--output', str(local / name)]
    with (local / (name + '.log')).open('x') as file:
        result = subprocess.run(command, stdout=file, stderr=subprocess.STDOUT)
    manifest_path = local / name / 'manifest.json'
    data = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
    record = {'name': name, 'command': command, 'returncode': result.returncode,
              'status': data.get('status')}
    records.append(record)
    (local / 'followup-runs.json').write_text(json.dumps(records, indent=2) + '\n')
    print(json.dumps(record), flush=True)
    if data.get('status') != 'completed' or data.get('cleanup_errors'):
        raise RuntimeError('Fixture/cleanup failed; inspect before continuing')
spec = importlib.util.spec_from_file_location('summary', 'scripts/perf/summarize_architecture.py')
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)
profiles = [summary.profile(path) for path in sorted((local / 'profiles-after').glob('*.stacks.txt'))]
assert len(profiles) == 3
(local / 'profiles-after-summary.json').write_text(json.dumps({
    'diagnostic_rps_excluded': True, 'inclusive_groups_overlap': True, 'profiles': profiles}, indent=2) + '\n')
raise SystemExit(int(any(record['returncode'] for record in records)))
