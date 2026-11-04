"""Use a matching diagnostic build to reproduce the per-request warning."""
import hashlib,json,subprocess,sys,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-retry-buffer';before=local/'before';deadline=time.monotonic()+45*60
while time.monotonic()<deadline:
    p=before/'metadata.json'
    if p.exists():
        m=json.loads(p.read_text())
        if 'build_returncode' in m:
            assert m['build_returncode']==0
            break
    time.sleep(5)
else:raise TimeoutError('No baseline build')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
for n,h in m['source_sha256'].items():assert sha(root/n)==h,n
a=json.loads((root/'.local/h3-recv-ablation/metadata.json').read_text())
cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py','--target','edge','--tls',str(root/'.local/perf-next/tls'),'--client',a['binaries']['client']['path'],'--controller',a['binaries']['controller']['path'],'--edge-binary',m['binary']['path'],'--output',str(before/'probe'),'--protocols','h3','--states','off','--connections','reuse','--seconds','3','--repeats','1','--concurrency','32','--idle-reference-seconds','2']
record={'command':cmd,'driver_sha256':sha(Path(__file__)),'binary_sha256':sha(Path(m['binary']['path'])),'capacity_comparison':False,'profile':m['profile']}
with (before/'probe.log').open('x') as log:record['returncode']=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
x=json.loads((before/'probe/manifest.json').read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
record['case_statuses']=[c['status'] for c in x['cases']];record['native_errors']=sum(c['native']['failed'] for c in x['cases']);record['warning_count']=(before/'probe/edge.log').read_text().count('Upstream body is already finished')
(before/'probe-completed.json').write_text(json.dumps(record,indent=2)+'\n')
assert record['returncode']==0 and record['native_errors']==0 and record['warning_count']>0
for n,h in m['source_sha256'].items():assert sha(root/n)==h,n
print('Baseline warning reproduced',record['warning_count'],flush=True)
