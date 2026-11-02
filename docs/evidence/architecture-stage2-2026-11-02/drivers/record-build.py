"""Run only after the production build exits successfully."""
import hashlib
import json
from pathlib import Path
import subprocess
import time

root = Path.cwd()
local = root / '.local/architecture-stage2'
def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as file:
        while chunk := file.read(1024 * 1024):
            h.update(chunk)
    return h.hexdigest()
source = json.loads((local / 'candidate-source.json').read_text())['files']
for path, expected in source.items():
    assert sha(root / path) == expected, path
binaries = {label: {'path': str(path.resolve()), 'sha256': sha(path)} for label, path in {
    'baseline': local / 'baseline-Zelvik-Edge',
    'candidate': Path('/workspace/edge/target/release/Zelvik-Edge'),
    'controller': Path('/workspace/edge/target/release/examples/issue34-controller'),
    'client': root / '.local/protocol-state-matrix/protocol-state-client',
}.items()}
assert binaries['baseline']['sha256'] == '64d0dc7bae0e0ebcd8b52862c46ad7bb00d038f1f8cf4e22fd34a2fe4ab563ff'
assert binaries['controller']['sha256'] == 'cb54275745f28e237822717e17f10a601f13724f41cace7dbb193c38c037299e'
assert binaries['candidate']['sha256'] != binaries['baseline']['sha256']
data = {'recorded_at': time.time(), 'binaries': binaries, 'source_sha256': source,
        'source_revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'source_date_epoch': 1793566871,
        'rust_version': subprocess.check_output(['rustc', '--version'], text=True).strip(),
        'rustflags': '-C target-cpu=x86-64-v2',
        'release_profile': {'opt_level': 3, 'lto': 'fat', 'codegen_units': 1, 'panic': 'abort'},
        'jobs': 2, 'test_threads': 2, 'parent_cpu_quota_cores': 4,
        'parent_limits': {name: Path('/sys/fs/cgroup', name).read_text().strip()
                          for name in ['cpu.max', 'memory.max']}}
(local / 'build-metadata.json').write_text(json.dumps(data, indent=2) + '\n')
print(json.dumps({'binaries': binaries, 'source_files': len(source)}, indent=2))
