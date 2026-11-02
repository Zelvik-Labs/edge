"""Start measurements only after every serialized build/check has exited."""
import json,os,subprocess,sys,time
from pathlib import Path
local=Path('.local/h3-stage2');deadline=time.monotonic()+75*60
while time.monotonic()<deadline:
    if (local/'checks-completed.json').exists():
        result=json.loads((local/'checks-completed.json').read_text())
        if result['returncode']:raise RuntimeError('Checks failed; no benchmark started')
        break
    time.sleep(5)
else:raise TimeoutError('Checks did not complete')
bench=[]
for line in (local/'microbench-build.log').read_text().splitlines():
    try:item=json.loads(line)
    except json.JSONDecodeError:continue
    if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='hotpath_bench' and item.get('executable'):
        bench.append(Path(item['executable']))
assert len(bench)==1,bench
with (local/'microbench.log').open('x') as log:
    subprocess.run(['taskset','-c','0,1',str(bench[0]),'h3_governor_reads','--bench','--sample-size','30','--measurement-time','3','--warm-up-time','1'],env={**os.environ,'CRITERION_HOME':str((local/'criterion').resolve())},stdout=log,stderr=subprocess.STDOUT,check=True)
subprocess.run([sys.executable,str(local/'run-suite.py')],check=True)
subprocess.run([sys.executable,str(local/'profile-h3.py')],check=True)
print('H3 stage 2 measurements complete',flush=True)
