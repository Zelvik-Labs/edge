"""Keep diagnostic proof separate from release capacity evidence."""
import difflib,hashlib,json,shutil
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-retry-buffer';dest=root/'docs/evidence/h3-retry-buffer-2026-11-02';dest.mkdir(parents=True,exist_ok=True)
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def save(p,x):p.write_text(json.dumps(x,indent=2,ensure_ascii=False)+'\n')
before=json.loads((local/'before/metadata.json').read_text());after=json.loads((local/'after/metadata.json').read_text());red=json.loads((local/'red-result.json').read_text());done=json.loads((local/'after/completed.json').read_text())
assert done['returncode']==0 and before['profile']==after['profile']
assert red['returncode']==101 and len(red['failed'])==2 and len(red['passed'])==1
for name,m in [('before',before),('after',after)]:
    for n,h in m['source_sha256'].items():assert sha(Path(m['source_snapshot_root'])/n)==h,n
    assert sha(Path(m['binary']['path']))==m['binary']['sha256']
    (dest/name).mkdir(exist_ok=True);save(dest/name/'metadata.json',m)
    for p in (local/name).glob('*.log'):shutil.copy2(p,dest/name/p.name)
for n,h in red['source_sha256'].items():
    p=local/'red-source'/n if n in ['src/h3_downstream.rs','src/proxy.rs'] else Path(before['source_snapshot_root'])/n
    assert sha(p)==h,n
assert {n for n,h in after['source_sha256'].items() if before['source_sha256'][n]!=h}=={'src/h3_downstream.rs','src/proxy.rs'}
for n in ['implementation.patch','red-result.json','red-tests.log','red-stage.log','build-before.log','probe-before.log']:
    shutil.copy2(local/n,dest/n)
test_patch=[]
for n in ['src/h3_downstream.rs','src/proxy.rs']:
    test_patch.extend(difflib.unified_diff((Path(before['source_snapshot_root'])/n).read_text().splitlines(True),(local/'red-source'/n).read_text().splitlines(True),fromfile='before/'+n,tofile='regression-tests/'+n))
(dest/'regression-tests.patch').write_text(''.join(test_patch))
for n in ['checks.json','test-results.json','completed.json','functional-index.json','protocol-regression-completed.json']:shutil.copy2(local/'after'/n,dest/'after'/n)
bp=json.loads((local/'before/probe-completed.json').read_text());assert bp['returncode']==0 and bp['warning_count']>0 and bp['native_errors']==0
shutil.copy2(local/'before/probe-completed.json',dest/'before/probe-completed.json')
entries=[{'phase':'before','name':'probe','manifest':str(local/'before/probe/manifest.json')},*[{'phase':'after',**r} for r in json.loads((local/'after/functional-index.json').read_text())],{'phase':'after',**json.loads((local/'after/protocol-regression-completed.json').read_text())}]
runtime=[];raw=[];portable=[]
for r in entries:
    p=Path(r['manifest']);m=json.loads(p.read_text());assert m['status']=='completed' and not m.get('cleanup_errors')
    binary=before['binary'] if r['phase']=='before' else after['binary'];assert binary['sha256'] in m['binaries'].values()
    assert all(c['status']=='passed' and c['native']['failed']==0 for c in m['cases'])
    target=dest/r['phase']/r['name'];target.mkdir(exist_ok=True);shutil.copy2(p,target/'manifest.json')
    portable.append({**r,'manifest':str((target/'manifest.json').relative_to(dest))})
    for c in m['cases']:
        runtime.append({'phase':r['phase'],'dataset':r['name'],'protocol':c['protocol'],'cache':c['cache'],'connection':c['connection'],'status':c['status'],'successful':c['native']['successful'],'failed':c['native']['failed'],'new_connections':c['native']['new_connections'],'origin_requests':c['origin_requests']})
    for f in p.parent.iterdir():
        if f.is_file() and f.name.endswith(('.log','.stderr','.tsv','.tsv.gz','.headers','.command.json')):raw.append(sha(f)+'  '+str(f.relative_to(root)))
off=json.loads((local/'after/functional-index.json').read_text())[0];assert off['name']=='probe-off' and off['last_case_empty_final_body_warnings']==0
save(dest/'functional-index.portable.json',portable);save(dest/'functional-results.json',runtime)
save(dest/'warning-comparison.json',{'before':{'warnings':bp['warning_count'],'successful':sum(r['successful'] for r in runtime if r['phase']=='before'),'native_errors':0},'after':{'warnings':0,'successful':sum(r['successful'] for r in runtime if r['phase']=='after' and r['dataset']=='probe-off'),'native_errors':0},'matching_profile':before['profile'],'capacity_comparison':False,'release_capacity_for_this_fix_completed':False})
(dest/'raw-files.sha256').write_text('\n'.join(sorted(raw))+'\n')
a=json.loads((root/'.local/h3-recv-ablation/metadata.json').read_text());save(dest/'harness-sha256.json',a['harness_sha256'])
(dest/'drivers').mkdir(exist_ok=True)
for p in local.glob('*.py'):shutil.copy2(p,dest/'drivers'/p.name)
save(dest/'tool-sha256.json',{str(p.relative_to(root)):sha(p) for p in local.glob('*.py')})
print({'before_functional':sum(r['phase']=='before' for r in runtime),'after_functional':sum(r['phase']=='after' for r in runtime),'before_warnings':bp['warning_count'],'after_warnings':0,'raw_hashes':len(raw),'tests':done['unit_tests']})
