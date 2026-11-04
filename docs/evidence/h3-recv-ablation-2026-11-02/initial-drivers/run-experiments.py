"""Compare three H3 implementations with balanced per-case order."""
import hashlib,json,shutil,subprocess,sys,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-recv-ablation';old=root/'.local/h3-stage2'
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
sources=json.loads((local/'candidate-source.json').read_text())
for name,digest in sources.items():assert sha(root/name)==digest,name
snapshot=local/'measured-source';snapshot.mkdir(exist_ok=False)
for name in sources:
    p=snapshot/name;p.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(root/name,p)
release=Path('/workspace/edge/target/release/Zelvik-Edge');candidate=local/'lean-Zelvik-Edge';shutil.copy2(release,candidate)
original=json.loads((old/'build-metadata.json').read_text())
baseline=Path(original['binaries']['baseline']['path']);batched=Path(original['binaries']['candidate']['path'])
client=root/'.local/protocol-state-matrix/protocol-state-client';controller=Path(original['binaries']['controller']['path'])
assert sha(candidate) not in [sha(baseline),sha(batched)]
binaries={name:{'path':str(p),'sha256':sha(p)} for name,p in [('baseline',baseline),('batched',batched),('candidate',candidate),('client',client),('controller',controller)]}
metadata={'source_sha256':sources,'source_snapshot_root':str(snapshot),'binaries':binaries,'build':original['build'],'difference':'candidate restores original single-datagram receive; lean governor and pressure logic identical to batched','harness_sha256':{name:sha(root/name) for name in original['harness_sha256']},'local_driver_sha256':{str(p.relative_to(root)):sha(p) for p in local.glob('*.py')},'production_deployed':False}
(local/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
common=['--target','edge','--tls',str(root/'.local/perf-next/tls'),'--client',str(client),'--controller',str(controller),'--repeats','1','--concurrency','32','--idle-reference-seconds','2']
out=local/'comparison';out.mkdir(exist_ok=False);index={'status':'running','runs':[],'seconds':10,'order':'rotate baseline/lean/batched within each state across three rounds'}
def save():(out/'index.json').write_text(json.dumps(index,indent=2)+'\n')
save()
orders=[['baseline','candidate','batched'],['batched','baseline','candidate'],['candidate','batched','baseline']]
for repeat,labels in enumerate(orders):
    for state in ['hit','off']:
        for label in labels:
            directory=out/f'{repeat:02d}-{state}-{label}'
            cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py',*common,'--edge-binary',binaries[label]['path'],'--output',str(directory),'--protocols','h3','--states',state,'--connections','reuse','--seconds','10']
            run={'label':label,'repeat':repeat,'state':state,'command':cmd,'manifest':str(directory/'manifest.json'),'started_at':time.time()};index['runs'].append(run);save()
            with (out/(directory.name+'.log')).open('x') as log:run['returncode']=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
            m=json.loads((directory/'manifest.json').read_text());assert m['status']=='completed' and not m.get('cleanup_errors')
            assert binaries[label]['sha256'] in m['binaries'].values()
            run['status']=m['status'];run['case_statuses']=[c['status'] for c in m['cases']];save()
            assert all(c['status']=='passed' for c in m['cases']),directory
            print(directory.name,run['case_statuses'],flush=True)
index['status']='completed';index['all_cases_passed']=True;save()
functions=[('smoke',['--states','hit','miss','off','--connections','reuse','--seconds','3','--cold-requests','128']),('large',['--states','hit','miss','--connections','reuse','--seconds','3','--payload-size','131072','--cold-requests','128']),('fresh',['--states','hit','off','--connections','fresh','--fresh-requests','2048','--seconds','10'])]
for name,flags in functions:
    cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py',*common,'--edge-binary',str(candidate),'--output',str(local/name),'--protocols','h3',*flags]
    with (local/(name+'.log')).open('x') as log:code=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
    m=json.loads((local/name/'manifest.json').read_text());assert code==0 and m['status']=='completed' and not m.get('cleanup_errors')
    assert all(c['status']=='passed' for c in m['cases']),name
    print(name,[c['status'] for c in m['cases']],flush=True)
for name,digest in sources.items():assert sha(root/name)==digest,name
print('Receive ablation measurements complete',flush=True)
