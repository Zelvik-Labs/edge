"""Verify H1/H2 after the shared body filter changes, without overlapping jobs."""
import hashlib,json,subprocess,sys,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-retry-buffer';after=local/'after';deadline=time.monotonic()+90*60
while time.monotonic()<deadline:
    log=(local/'after-stage.log').read_text()
    if 'Retry buffer fix validated' in log:break
    if 'Traceback (most recent call last)' in log:raise RuntimeError('Primary verification failed')
    time.sleep(5)
else:raise TimeoutError('Primary verification incomplete')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
m=json.loads((after/'metadata.json').read_text());assert json.loads((after/'completed.json').read_text())['returncode']==0
for n,h in m['source_sha256'].items():assert sha(root/n)==h,n
a=json.loads((root/'.local/h3-recv-ablation/metadata.json').read_text())
cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py','--target','edge','--tls',str(root/'.local/perf-next/tls'),'--client',a['binaries']['client']['path'],'--controller',a['binaries']['controller']['path'],'--edge-binary',m['binary']['path'],'--output',str(after/'protocols-h1h2'),'--protocols','h1','h2','--states','hit','miss','off','--connections','reuse','--seconds','3','--cold-requests','128','--repeats','1','--concurrency','32','--idle-reference-seconds','2']
with (after/'protocols-h1h2.log').open('x') as log:code=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
x=json.loads((after/'protocols-h1h2/manifest.json').read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
r={'name':'protocols-h1h2','command':cmd,'manifest':str(after/'protocols-h1h2/manifest.json'),'returncode':code,'case_statuses':[c['status'] for c in x['cases']],'native_errors':sum(c['native']['failed'] for c in x['cases']),'driver_sha256':sha(Path(__file__))}
(after/'protocol-regression-completed.json').write_text(json.dumps(r,indent=2)+'\n')
assert code==0 and len(r['case_statuses'])==6 and all(s=='passed' for s in r['case_statuses']) and r['native_errors']==0,r
assert m['binary']['sha256'] in x['binaries'].values()
for n,h in m['source_sha256'].items():assert sha(root/n)==h,n
print('Shared H1/H2 body-filter regression complete',r['case_statuses'],flush=True)
