"""Archive the controlled receive comparison with frozen source and binaries."""
import copy,hashlib,json,shutil,subprocess,sys
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-recv-ablation';dest=root/'docs/evidence/h3-recv-ablation-2026-11-02';dest.mkdir(parents=True,exist_ok=True)
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def save(p,x):p.write_text(json.dumps(x,indent=2,ensure_ascii=False)+'\n')
m=json.loads((local/'metadata.json').read_text())
for n,h in m['source_sha256'].items():assert sha(Path(m['source_snapshot_root'])/n)==h,n
for n,x in m['binaries'].items():assert sha(Path(x['path']))==x['sha256'],n
for key in ['harness_sha256','local_driver_sha256']:
    for n,h in m[key].items():assert sha(root/n)==h,n
save(dest/'metadata.json',m)
for pattern in ['test-*.log','clippy.log','release.log','checks.json','test-results.json','storage-*.json','*-source.json','batch-removal.patch']:
    for p in local.glob(pattern):shutil.copy2(p,dest/p.name)
index=json.loads((local/'comparison/index.json').read_text());assert index['status']=='completed'
directory=dest/'comparison';(directory/'manifests').mkdir(parents=True,exist_ok=True)
shutil.copy2(local/'comparison/index.json',directory/'index.json');portable=copy.deepcopy(index);rawdirs=[]
for r in portable['runs']:
    p=Path(r['manifest']);x=json.loads(p.read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
    assert all(c['fresh_instance'] and c['status']=='passed' and c['native']['failed']==0 for c in x['cases'])
    target=directory/'manifests'/(p.parent.name+'.json');shutil.copy2(p,target);r['manifest']=str(target.relative_to(directory));rawdirs.append(p.parent)
save(directory/'index.portable.json',portable)
subprocess.run([sys.executable,'scripts/perf/summarize_architecture_candidates.py','--index',str(directory/'index.portable.json'),'--output',str(directory)],check=True)
functions={}
for name in ['smoke','large','fresh']:
    p=local/name/'manifest.json';x=json.loads(p.read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
    assert all(c['status']=='passed' and c['native']['failed']==0 for c in x['cases'])
    (dest/name).mkdir(exist_ok=True);shutil.copy2(p,dest/name/'manifest.json');rawdirs.append(p.parent)
    functions[name]=[{'protocol':c['protocol'],'state':c['cache'],'status':c['status'],'success':c['native']['successful'],'failed':c['native']['failed'],'new_connections':c['native']['new_connections'],'origin_requests':c['origin_requests']} for c in x['cases']]
save(dest/'functional-validation.json',functions)
raw=[]
for d in rawdirs:
    for p in d.iterdir():
        if p.is_file() and p.name.endswith(('.log','.stderr','.tsv','.tsv.gz','.headers','.command.json')):raw.append(sha(p)+'  '+str(p.relative_to(root)))
(dest/'raw-files.sha256').write_text('\n'.join(sorted(raw))+'\n')
(dest/'drivers').mkdir(exist_ok=True)
for p in local.glob('*.py'):shutil.copy2(p,dest/'drivers'/p.name)
print({'capacity_cases':len(index['runs']),'functional_cases':sum(len(v) for v in functions.values()),'raw_hashes':len(raw)})
