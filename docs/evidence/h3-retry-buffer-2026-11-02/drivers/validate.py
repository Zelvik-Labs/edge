"""Verify the red/green sequence, matched diagnostics, and frozen source."""
import hashlib,json,re,subprocess
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-retry-buffer';base=root/'docs/evidence/h3-retry-buffer-2026-11-02'
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
for name in ['before','after']:
    m=json.loads((base/name/'metadata.json').read_text())
    for n,h in m['source_sha256'].items():assert sha(Path(m['source_snapshot_root'])/n)==h,n
    assert sha(Path(m['binary']['path']))==m['binary']['sha256']
after=json.loads((base/'after/metadata.json').read_text())
before=json.loads((base/'before/metadata.json').read_text())
assert before['profile']==after['profile'] and before['binary']['sha256']!=after['binary']['sha256']
assert after['driver_sha256']==sha(local/'run-after.py')
for n,h in after['source_sha256'].items():assert sha(root/n)==h,n
for name in ['harness-sha256.json','tool-sha256.json']:
    for n,h in json.loads((base/name).read_text()).items():assert sha(root/n)==h,n
red=json.loads((base/'red-result.json').read_text());assert red['returncode']==101 and len(red['failed'])==2 and len(red['passed'])==1
checks=json.loads((base/'after/checks.json').read_text());assert len(checks)==8 and all(r['returncode']==0 for r in checks)
tests=json.loads((base/'after/test-results.json').read_text());assert tests['passed_unique']>=411 and tests['ignored_unique']==1
old_names=set()
for p in (root/'.local/h3-stage2').glob('test-*.log'):
    old_names.update(re.findall(r'^test (\S+) \.\.\. ok',p.read_text(),re.M))
assert len(old_names)==408 and old_names.issubset(set(tests['test_names']))
assert set(red['failed']+red['passed']).issubset(set(tests['test_names']))
for p in base.rglob('*'):
    if p.is_file():
        assert p.suffix not in ['.pem','.key','.data','.pyc'] and p.name!='overrides.json',p
        assert not re.search(rb'(?m)^-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----\r?$',p.read_bytes()),p
for p in base.rglob('*.py'):compile(p.read_text(),str(p),'exec')
for p in [root/'docs/performance-h3-retry-buffer-2026-11-02.md',base/'README.md']:
    t=p.read_text();assert '<!--' not in t,p
    for target in re.findall(r'\]\(([^)]+)\)',t):
        if '://' not in target and not target.startswith('#'):assert (p.parent/target.split('#')[0]).exists(),(p,target)
rows=json.loads((base/'functional-results.json').read_text());assert len(rows)==12 and sum(r['phase']=='after' for r in rows)==11
assert all(r['status']=='passed' and r['failed']==0 for r in rows)
w=json.loads((base/'warning-comparison.json').read_text());assert w['before']['warnings']>0 and w['after']['warnings']==0 and w['capacity_comparison'] is False
for r in json.loads((base/'functional-index.portable.json').read_text()):
    p=base/r['manifest'];m=json.loads(p.read_text());assert m['status']=='completed' and not m.get('cleanup_errors')
    assert all(c['status']=='passed' and c['native']['failed']==0 for c in m['cases'])
with (local/'raw-hash-validation.log').open('w') as log:subprocess.run(['sha256sum','-c',str(base/'raw-files.sha256')],stdout=log,check=True)
v={'source_files_per_version':len(after['source_sha256']),'binary_hashes':2,'red_expected_failures':2,'passed_unique':tests['passed_unique'],'ignored_unique':1,'functional_contracts':12,'before_warning_count':w['before']['warnings'],'after_warning_count':0,'raw_hashes_match':True,'live_source_matches_after':True,'release_capacity_completed':False,'no_runtime_secrets':True}
(base/'archive-validation.json').write_text(json.dumps(v,indent=2)+'\n')
files=sorted(p for p in base.rglob('*') if p.is_file() and p.name!='artifacts.sha256')
(base/'artifacts.sha256').write_text('\n'.join(sha(p)+'  '+str(p.relative_to(base)) for p in files)+'\n')
with (local/'artifact-hash-validation.log').open('w') as log:subprocess.run(['sha256sum','-c','artifacts.sha256'],cwd=base,stdout=log,check=True)
print(json.dumps(v))
