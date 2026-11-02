"""Resolve the noisy reuse regression with per-case Edge alternation."""
import json,subprocess,sys,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';out=local/'recheck-reuse';out.mkdir(exist_ok=False)
index={'status':'running','runs':[],'purpose':'follow-up to contradictory reuse CPU results; all original samples retained','seconds':10}
def save():(out/'index.json').write_text(json.dumps(index,indent=2)+'\n')
save()
for repeat in range(3):
    for state in ['hit','off']:
        labels=['baseline','candidate'] if repeat%2==0 else ['candidate','baseline']
        for label in labels:
            directory=out/f'{repeat:02d}-{state}-{label}'
            binary=local/'baseline-Zelvik-Edge' if label=='baseline' else Path('/workspace/edge/target/release/Zelvik-Edge')
            cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py','--target','edge','--edge-binary',str(binary),'--tls',str(root/'.local/perf-next/tls'),'--client',str(root/'.local/protocol-state-matrix/protocol-state-client'),'--controller','/workspace/edge/target/release/examples/issue34-controller','--output',str(directory),'--protocols','h3','--states',state,'--connections','reuse','--repeats','1','--seconds','10','--idle-reference-seconds','2']
            run={'label':label,'repeat':repeat,'state':state,'command':cmd,'manifest':str(directory/'manifest.json'),'started_at':time.time()};index['runs'].append(run);save()
            with (out/(directory.name+'.log')).open('x') as log:run['returncode']=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
            m=json.loads((directory/'manifest.json').read_text());assert m['status']=='completed' and not m.get('cleanup_errors')
            run['status']=m['status'];run['case_statuses']=[c['status'] for c in m['cases']];save()
            assert all(c['status']=='passed' for c in m['cases']),directory
            print(directory.name,run['case_statuses'],flush=True)
index['status']='completed';index['all_cases_passed']=all(r['returncode']==0 for r in index['runs']);save()
