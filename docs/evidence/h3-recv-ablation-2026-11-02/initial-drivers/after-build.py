"""Wait for every check/build to exit before starting the controlled comparison."""
import json,subprocess,sys,time
from pathlib import Path
local=Path('.local/h3-recv-ablation');deadline=time.monotonic()+90*60
while time.monotonic()<deadline:
    if (local/'checks-completed.json').exists():
        result=json.loads((local/'checks-completed.json').read_text())
        if result['returncode']:raise RuntimeError('Ablation build/check failed; no measurement started')
        break
    time.sleep(5)
else:raise TimeoutError('Ablation build did not finish')
raise SystemExit(subprocess.run([sys.executable,str(local/'run-experiments.py')]).returncode)
