"""Run one production target/case at a time after the frozen release finishes."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time

root = Path.cwd()
local = root / '.local/h3-stage2'
binary = Path('/workspace/edge/target/release/Zelvik-Edge').resolve()
controller = Path('/workspace/edge/target/release/examples/issue34-controller').resolve()
client = root / '.local/protocol-state-matrix/protocol-state-client'
baseline = local / 'baseline-Zelvik-Edge'
def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as source:
        while data := source.read(1024 * 1024): h.update(data)
    return h.hexdigest()

sources = json.loads((local / 'candidate-source.json').read_text())
for name, digest in sources.items(): assert sha(root / name) == digest, name
assert sha(binary) != sha(baseline), 'Release still points to baseline'
metadata = {'recorded_at': time.time(), 'source_sha256': sources,
    'binaries': {name: {'path': str(path), 'sha256': sha(path)} for name, path in [
        ('baseline', baseline), ('candidate', binary), ('controller', controller), ('client', client)]},
    'harness_sha256': {str(p.relative_to(root)): sha(p) for p in [
        root/'scripts/perf/protocol_state_matrix.py', root/'scripts/perf/compare_architecture_candidates.py',
        root/'scripts/perf/summarize_architecture_candidates.py', root/'benches/support/protocol_state_client.c']},
    'build': {'rust': '1.99.1', 'target_cpu': 'x86-64-v2', 'opt_level': 3,
              'lto': 'fat', 'codegen_units': 1, 'panic': 'abort', 'jobs': 2},
    'production_deployed': False, 'long_stability_completed': False}
(local / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
common = ['--tls', str(root/'.local/perf-next/tls'), '--client', str(client),
          '--controller', str(controller)]
matrix = [sys.executable, 'scripts/perf/protocol_state_matrix.py', '--target', 'edge',
          '--edge-binary', str(binary), '--repeats', '1', *common]
compare = [sys.executable, 'scripts/perf/compare_architecture_candidates.py',
    '--baseline', str(baseline), '--candidate', str(binary), *common]
jobs = [
    ('smoke', [*matrix, '--protocols', 'h3', '--states', 'hit', 'miss', 'off',
               '--connections', 'reuse', '--seconds', '3', '--cold-requests', '128'], True),
    ('compare-h3', [*compare, '--protocols', 'h3', '--states', 'hit', 'miss', 'off',
                    '--connections', 'reuse', '--multiplex', '--seconds', '5',
                    '--cold-requests', '2048', '--repeats', '3'], False),
    ('compare-fresh', [*compare, '--protocols', 'h3', '--states', 'hit', 'miss', 'off',
                       '--connections', 'fresh', '--fresh-requests', '512',
                       '--seconds', '5', '--repeats', '1'], False),
    ('compare-32k', [*compare, '--protocols', 'h3', '--states', 'hit', 'miss', 'off',
                     '--connections', 'reuse', '--payload-size', '32768',
                     '--cold-requests', '512', '--seconds', '5', '--repeats', '1'], False),
    ('candidate-fresh-long', [*matrix, '--protocols', 'h3', '--states', 'hit', 'miss', 'off',
                              '--connections', 'fresh', '--fresh-requests', '2048',
                              '--seconds', '5', '--idle-reference-seconds', '2'], False),
    ('candidate-h1-h2', [*matrix, '--protocols', 'h1', 'h2', '--states', 'hit', 'miss', 'off',
                         '--connections', 'reuse', '--seconds', '5', '--cold-requests', '2048',
                         '--idle-reference-seconds', '2'], False),
    ('functional-128k', [*matrix, '--protocols', 'h3', '--states', 'hit', 'miss',
                          '--connections', 'reuse', '--payload-size', '131072',
                          '--cold-requests', '128', '--seconds', '3'], True),
]
runs = []
for name, command, functional in jobs:
    command += ['--output', str(local/name)]
    started = time.time()
    with (local/(name+'.log')).open('x') as log:
        code = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT).returncode
    entry = {'name': name, 'command': command, 'returncode': code,
             'functional_only': functional, 'started_at': started, 'ended_at': time.time()}
    if (local/name/'manifest.json').exists():
        manifests = [json.loads((local/name/'manifest.json').read_text())]
        entry['manifest'] = str(local/name/'manifest.json')
    else:
        index = json.loads((local/name/'index.json').read_text())
        assert index['status'] == 'completed', name
        entry['index'] = str(local/name/'index.json')
        manifests = [json.loads(Path(run['manifest']).read_text()) for run in index['runs']]
    entry['statuses'] = [c['status'] for manifest in manifests for c in manifest['cases']]
    runs.append(entry)
    (local/'suite-runs.json').write_text(json.dumps(runs, indent=2)+'\n')
    print(json.dumps(entry), flush=True)
    assert all(m['status'] == 'completed' and not m.get('cleanup_errors') for m in manifests), name
    # Old baseline cache bypass and old connection failures are retained;
    # a new candidate failing a contract needs investigation before broadening.
    # Binaries are recorded as path -> digest by the matrix driver.
    candidate = [m for m in manifests if metadata['binaries']['candidate']['sha256'] in m.get('binaries', {}).values()]
    assert all(c['status'] == 'passed' for m in candidate for c in m['cases']), name
for name, digest in sources.items(): assert sha(root / name) == digest, name
assert sha(binary) == metadata['binaries']['candidate']['sha256']
print('H3 phase 2 suite complete', flush=True)
