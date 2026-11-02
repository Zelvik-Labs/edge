"""Write evidence only after every ablation run completes."""
import json,re,subprocess,sys,time
from pathlib import Path
local=Path('.local/h3-recv-ablation');deadline=time.monotonic()+120*60
while time.monotonic()<deadline:
    log=(local/'measurements.log').read_text()
    if 'Receive ablation measurements complete' in log:break
    if 'Traceback (most recent call last)' in log:raise RuntimeError('Ablation failed; investigate before summarizing')
    time.sleep(5)
else:raise TimeoutError('No completed ablation')
passed=set();ignored=set()
for p in local.glob('test-*.log'):
    for name,status in re.findall(r'^test (\S+) \.\.\. (ok|ignored)',p.read_text(),re.M):
        (passed if status=='ok' else ignored).add(name)
(local/'test-results.json').write_text(json.dumps({'passed_unique':len(passed),'ignored_unique':len(ignored),'test_names':sorted(passed),'ignored':sorted(ignored)},indent=2)+'\n')
for name in ['archive.py','render-report.py']:subprocess.run([sys.executable,str(local/name)],check=True)
print('Ablation report generated; select implementation and validate next',flush=True)
