"""Start the independent suite only after the release compiler has exited."""
from pathlib import Path
import subprocess
import sys
import time

log = Path('.local/h3-stage1/release-build-retry.log')
deadline = time.monotonic() + 45 * 60
while time.monotonic() < deadline:
    contents = log.read_text() if log.exists() else ''
    if 'budget exceeded' in contents or 'could not compile' in contents:
        raise RuntimeError('Release failed; inspect build log before measuring')
    if 'Finished `release`' in contents:
        break
    time.sleep(5)
else:
    raise TimeoutError('No completed release within the build observation window')
raise SystemExit(subprocess.run([sys.executable, '.local/h3-stage1/run-suite.py']).returncode)
