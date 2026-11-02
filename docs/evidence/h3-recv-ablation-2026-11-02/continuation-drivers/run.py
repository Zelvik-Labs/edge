"""Finish the preplanned matrix without retrying or hiding its failed case."""
import copy,hashlib,json,subprocess,sys,time
from pathlib import Path
root=Path.cwd();old=root/'.local/h3-recv-ablation';local=root/'.local/h3-recv-continuation';local.mkdir(exist_ok=True)
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
m=json.loads((old/'metadata.json').read_text())
for name,h in m['source_sha256'].items():assert sha(root/name)==h,name
for name,x in m['binaries'].items():assert sha(Path(x['path']))==x['sha256'],name
initial=json.loads((old/'comparison/index.json').read_text());assert len(initial['runs'])==16
assert initial['runs'][-1]['case_statuses']==['traffic-errors']
(local/'initial-index.json').write_text(json.dumps(initial,indent=2)+'\n')
meta={'driver_sha256':sha(Path(__file__)),'original_metadata_sha256':sha(old/'metadata.json'),'reason':'original strict runner stopped at case 16 with 12 curl_28 failures; finish only missing cases and validate both alternatives; no retries or timeout changes','functional_variants':['candidate','batched'],'functional_cases_per_variant':7,'failing_original_case_preserved':True}
(local/'continuation-metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
common=['--target','edge','--tls',str(root/'.local/perf-next/tls'),'--client',m['binaries']['client']['path'],'--controller',m['binaries']['controller']['path'],'--repeats','1','--concurrency','32','--idle-reference-seconds','2']
index=copy.deepcopy(initial);index['continuation_metadata']=str(local/'continuation-metadata.json');index['status']='running'
def save():(local/'comparison-index.json').write_text(json.dumps(index,indent=2)+'\n')
save()
for label in ['baseline','batched']:
    directory=local/f'02-off-{label}'
    cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py',*common,'--edge-binary',m['binaries'][label]['path'],'--output',str(directory),'--protocols','h3','--states','off','--connections','reuse','--seconds','10']
    r={'label':label,'repeat':2,'state':'off','command':cmd,'manifest':str(directory/'manifest.json'),'started_at':time.time()};index['runs'].append(r);save()
    with (local/(directory.name+'.log')).open('x') as log:r['returncode']=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
    x=json.loads((directory/'manifest.json').read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
    assert m['binaries'][label]['sha256'] in x['binaries'].values()
    r['status']=x['status'];r['case_statuses']=[c['status'] for c in x['cases']];save()
    print(directory.name,r['case_statuses'],flush=True)
index['status']='completed';index['all_cases_passed']=all(r['case_statuses']==['passed'] for r in index['runs']);save()
functions=[('smoke',['--states','hit','miss','off','--connections','reuse','--seconds','3','--cold-requests','128']),('large',['--states','hit','miss','--connections','reuse','--seconds','3','--payload-size','131072','--cold-requests','128']),('fresh',['--states','hit','off','--connections','fresh','--fresh-requests','2048','--seconds','10'])]
validation=[]
for label in ['candidate','batched']:
    for name,flags in functions:
        directory=local/f'{label}-{name}'
        cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py',*common,'--edge-binary',m['binaries'][label]['path'],'--output',str(directory),'--protocols','h3',*flags]
        with (local/(directory.name+'.log')).open('x') as log:code=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
        x=json.loads((directory/'manifest.json').read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
        assert m['binaries'][label]['sha256'] in x['binaries'].values()
        validation.append({'label':label,'name':name,'returncode':code,'manifest':str(directory/'manifest.json'),'case_statuses':[c['status'] for c in x['cases']]})
        (local/'functional-index.json').write_text(json.dumps(validation,indent=2)+'\n')
        print(directory.name,validation[-1]['case_statuses'],flush=True)
for name,h in m['source_sha256'].items():assert sha(root/name)==h,name
print('Continuation complete; original failure retained',flush=True)
raise SystemExit(0 if index['all_cases_passed'] and all(all(s=='passed' for s in r['case_statuses']) for r in validation) else 1)
