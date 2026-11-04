"""Retry lossy diagnostics after capacity and policy checks finish."""
import json,subprocess,sys,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';deadline=time.monotonic()+25*60
while time.monotonic()<deadline:
    text=(local/'postprocessing.log').read_text()
    if 'ValueError: lost samples or missing loss counter' in text:break
    if 'Traceback (most recent call last)' in text:raise RuntimeError('Unexpected postprocessing failure; inspect before recovery')
    time.sleep(5)
else:raise TimeoutError('Awaiting the rejected initial profile')
assert json.loads((local/'recheck-reuse/index.json').read_text())['status']=='completed'
assert (local/'cloud-cache-policy.json').exists(),'Policy validation must have completed first'
for script in ['profile-h3-retry.py','summarize-profiles.py','record-extra-provenance.py','archive.py','render-results.py','finish-report.py']:
    subprocess.run([sys.executable,str(local/script)],check=True)
print('Report generated from qualified profiles; manual review and final validation remain',flush=True)
