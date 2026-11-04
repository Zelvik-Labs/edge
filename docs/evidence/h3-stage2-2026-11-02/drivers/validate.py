"""Verify frozen inputs and portable phase 2 evidence; retain all failures."""
import hashlib,json,re,subprocess,sys
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';base=root/'docs/evidence/h3-stage2-2026-11-02'
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
m=json.loads((base/'metadata.json').read_text())
source_root=Path(m.get('source_snapshot_root',root))
for name,digest in m['source_sha256'].items():assert sha(source_root/name)==digest,name
for name,value in m['binaries'].items():assert sha(Path(value['path']))==value['sha256'],name
for name,digest in m['harness_sha256'].items():assert sha(root/name)==digest,name
for key in ['diagnostic_harness_sha256','local_driver_sha256']:
    for name,digest in m[key].items():assert sha(root/name)==digest,name
assert sha(Path(m['microbenchmark']['path']))==m['microbenchmark']['sha256']
for name,digest in m['cloud_cache_policy']['source_sha256'].items():assert sha(Path(name))==digest,name
assert Path('/workspace/.local/storage-profile').read_text().strip()=='cloud'
v=json.loads((base/'measurement-validation.json').read_text());assert v['capacity_cases']==117,v
assert v['candidate_cases']==39 and v['candidate_statuses']=={'passed':39} and v['candidate_native_failures']==0,v
assert all(g['statuses']=={'passed':3 if name=='smoke' else 2} and g['failed']==0 for name,g in v['functional'].items())
for p in base.rglob('index.portable.json'):
    x=json.loads(p.read_text());assert x['status']=='completed'
    for r in x['runs']:assert (p.parent/r['manifest']).is_file(),p
for p in [root/'docs/performance-h3-stage2-2026-11-02.md',root/'docs/performance-h3-tokio-quiche-evaluation-2026-11-02.md',base/'README.md']:
    text=p.read_text();assert '<!--' not in text,p
    for target in re.findall(r'\]\(([^)]+)\)',text):
        if '://' not in target and not target.startswith('#'):assert (p.parent/target.split('#')[0]).exists(),(p,target)
for p in base.rglob('*'):
    if p.is_file():
        assert p.suffix not in ['.pem','.key','.data','.pyc'],p
        assert p.name!='overrides.json',p
        assert not re.search(rb'(?m)^-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----\r?$',p.read_bytes()),p
for p in (base/'drivers').glob('*.py'):compile(p.read_text(),str(p),'exec')
with (local/'raw-hash-validation.log').open('w') as log:subprocess.run(['sha256sum','-c',str(base/'raw-files.sha256')],stdout=log,check=True)
for name in ['compare-h3','compare-fresh','compare-32k','recheck-reuse']:
    subprocess.run([sys.executable,'scripts/perf/summarize_architecture_candidates.py','--index',str(base/name/'index.portable.json'),'--output',str(local/('recomputed-'+name))],check=True)
    assert (local/('recomputed-'+name)/'results.json').read_bytes()==(base/name/'results.json').read_bytes()
validation={'frozen_files':len(m['source_sha256']),'source_root':str(source_root),'binary_hashes':len(m['binaries']),'all_new_contracts_passed':True,'all_failures_retained':True,'raw_hashes_match':True,'portable_summaries_recompute':True,'report_links_resolve':True,'runtime_secrets_excluded':True}
(base/'archive-validation.json').write_text(json.dumps(validation,indent=2)+'\n')
files=sorted(p for p in base.rglob('*') if p.is_file() and p.name!='artifacts.sha256')
(base/'artifacts.sha256').write_text('\n'.join(sha(p)+'  '+str(p.relative_to(base)) for p in files)+'\n')
with (local/'artifact-hash-validation.log').open('w') as log:subprocess.run(['sha256sum','-c','artifacts.sha256'],cwd=base,stdout=log,check=True)
print(json.dumps(validation))
