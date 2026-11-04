"""Verify evidence while requiring known failures to remain failures."""
import hashlib,json,re,subprocess,sys
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-recv-continuation';base=root/'docs/evidence/h3-recv-ablation-2026-11-02'
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
m=json.loads((base/'metadata.json').read_text())
for n,h in m['source_sha256'].items():assert sha(Path(m['source_snapshot_root'])/n)==h,n
for n,x in m['binaries'].items():assert sha(Path(x['path']))==x['sha256'],n
for key in ['harness_sha256','local_driver_sha256']:
    for n,h in m[key].items():assert sha(root/n)==h,n
for n,h in json.loads((base/'continuation-tools.json').read_text()).items():assert sha(root/n)==h,n
x=json.loads((base/'comparison/results.json').read_text());assert x['case_count']==18
assert sum(c['status']=='passed' for c in x['cases'])==17 and sum(c['failed'] for c in x['cases'])==12
bad=[c for c in x['cases'] if c['status']!='passed'];assert len(bad)==1 and bad[0]['label']=='candidate' and bad[0]['cache']=='off'
g=[g for g in x['groups'] if g['label']=='candidate' and g['cache']=='off'][0];assert g['passed']==2 and 'traffic_or_state_failure' in g['quality_flags']
f=json.loads((base/'functional-validation.json').read_text());assert sum(len(v) for v in f.values())==14
assert all(c['status']=='passed' and c['failed']==0 for rows in f.values() for c in rows)
for p in [root/'docs/performance-h3-receive-ablation-2026-11-02.md',base/'README.md']:
    t=p.read_text();assert '<!--' not in t,p
    for target in re.findall(r'\]\(([^)]+)\)',t):
        if '://' not in target and not target.startswith('#'):assert (p.parent/target.split('#')[0]).exists(),(p,target)
for p in base.rglob('*'):
    if p.is_file():
        assert p.suffix not in ['.pem','.key','.data','.pyc'] and p.name!='overrides.json',p
        assert not re.search(rb'(?m)^-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----\r?$',p.read_bytes()),p
for p in base.rglob('*.py'):compile(p.read_text(),str(p),'exec')
with (local/'raw-hash-validation.log').open('w') as log:subprocess.run(['sha256sum','-c',str(base/'raw-files.sha256')],stdout=log,check=True)
subprocess.run([sys.executable,'scripts/perf/summarize_architecture_candidates.py','--index',str(base/'comparison/index.portable.json'),'--output',str(local/'recomputed')],check=True)
assert (local/'recomputed/results.json').read_bytes()==(base/'comparison/results.json').read_bytes()
validation={'source_files':len(m['source_sha256']),'binary_hashes':len(m['binaries']),'capacity_cases':18,'capacity_passed':17,'native_errors_retained':12,'functional_contracts':14,'raw_hashes_match':True,'summary_recomputes':True,'no_runtime_secrets':True}
(base/'archive-validation.json').write_text(json.dumps(validation,indent=2)+'\n')
files=sorted(p for p in base.rglob('*') if p.is_file() and p.name!='artifacts.sha256')
(base/'artifacts.sha256').write_text('\n'.join(sha(p)+'  '+str(p.relative_to(base)) for p in files)+'\n')
with (local/'artifact-hash-validation.log').open('w') as log:subprocess.run(['sha256sum','-c','artifacts.sha256'],cwd=base,stdout=log,check=True)
print(json.dumps(validation))
