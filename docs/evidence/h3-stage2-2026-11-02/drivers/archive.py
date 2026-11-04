"""Archive phase 2 evidence without runtime keys or control overrides."""
from collections import Counter
import copy,hashlib,json,shutil,subprocess,sys
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';dest=root/'docs/evidence/h3-stage2-2026-11-02';dest.mkdir(parents=True,exist_ok=True)
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def save(p,data):p.write_text(json.dumps(data,indent=2,ensure_ascii=False)+'\n')
m=json.loads((local/'build-metadata.json').read_text())
source_root=Path(m.get('source_snapshot_root',root))
for name,digest in m['source_sha256'].items():assert sha(source_root/name)==digest,name
for name,value in m['binaries'].items():assert sha(Path(value['path']))==value['sha256'],name
for name,digest in m['harness_sha256'].items():assert sha(root/name)==digest,name
save(dest/'metadata.json',m)
for pattern in ['test*.log','python-tests.log','clippy.log','release-build*.log','release-observation.json','microbench*.log','microbench-artifact-ready.json','measurement-readiness.json','supplemental-build-cancellation.json','storage-*.json','cloud-cache-*.json','cloud-cache-policy.patch','*gc*.json','cleanup-*.log','*-source.json','stage2.patch','check-jobs.json','checks-first-attempt.json','test-results.json','initial-profile-quality.json','profiles-summary.json']:
    for p in local.glob(pattern):shutil.copy2(p,dest/p.name)
records=[];rawdirs=[]
for name in ['compare-h3','compare-fresh','compare-32k','recheck-reuse']:
    source=local/name/'index.json';index=json.loads(source.read_text());assert index['status']=='completed'
    directory=dest/name;(directory/'manifests').mkdir(parents=True,exist_ok=True)
    shutil.copy2(source,directory/'index.json');portable=copy.deepcopy(index)
    for run in portable['runs']:
        p=Path(run['manifest']);manifest=json.loads(p.read_text())
        assert manifest['status']=='completed' and not manifest.get('cleanup_errors')
        assert all(c.get('fresh_instance') for c in manifest['cases'])
        target=directory/'manifests'/(p.parent.name+'.json');shutil.copy2(p,target);run['manifest']=str(target.relative_to(directory))
        records.extend(dict(c,label=run['label'],round=run['repeat'],group=name) for c in manifest['cases']);rawdirs.append(p.parent)
    save(directory/'index.portable.json',portable)
    subprocess.run([sys.executable,'scripts/perf/summarize_architecture_candidates.py','--index',str(directory/'index.portable.json'),'--output',str(directory)],check=True)
functional={}
for name in ['smoke','functional-128k','candidate-fresh-long','candidate-h1-h2']:
    path=local/name/'manifest.json';manifest=json.loads(path.read_text());assert manifest['status']=='completed' and not manifest.get('cleanup_errors')
    directory=dest/name;directory.mkdir(exist_ok=True);shutil.copy2(path,directory/'manifest.json');rawdirs.append(path.parent)
    if name in ['smoke','functional-128k']:
        functional[name]={'statuses':dict(Counter(c['status'] for c in manifest['cases'])),'successful':sum(c.get('native',{}).get('successful',0) for c in manifest['cases']),'failed':sum(c.get('native',{}).get('failed',0) for c in manifest['cases'])}
    else:
        records.extend(dict(c,label='candidate',round=0,group=name) for c in manifest['cases'])
        save(directory/'index.portable.json',{'status':'completed','runs':[{'label':'candidate','repeat':0,'manifest':'manifest.json'}]})
        subprocess.run([sys.executable,'scripts/perf/summarize_architecture_candidates.py','--index',str(directory/'index.portable.json'),'--output',str(directory)],check=True)
for name in ['profiles-before','profiles-after','profiles-before-initial','profiles-after-initial','profiles-before-buffer-failed']:
    path=local/name/'manifest.json';manifest=json.loads(path.read_text());assert manifest['status']=='completed' and not manifest.get('cleanup_errors')
    directory=dest/name;directory.mkdir(exist_ok=True);shutil.copy2(path,directory/'manifest.json');rawdirs.append(path.parent)
    for p in path.parent.glob('*.perf-flat.txt'):shutil.copy2(p,directory/p.name)
raw=[]
for directory in rawdirs:
    for p in directory.iterdir():
        if p.is_file() and p.name.endswith(('.log','.stderr','.tsv','.tsv.gz','.headers','.command.json','.perf-command.json','.stacks.txt')):raw.append(sha(p)+'  '+str(p.relative_to(root)))
(dest/'raw-files.sha256').write_text('\n'.join(sorted(raw))+'\n')
candidate=[c for c in records if c['label']=='candidate']
save(dest/'measurement-validation.json',{'capacity_cases':len(records),'capacity_statuses':dict(Counter(c['status'] for c in records)),'candidate_cases':len(candidate),'candidate_statuses':dict(Counter(c['status'] for c in candidate)),'candidate_native_failures':sum(c.get('native',{}).get('failed',0) for c in candidate),'known_native_failures':sum(c.get('native',{}).get('failed',0) for c in records),'cases_without_native_result':sum('native' not in c for c in records),'functional':functional,'raw_hashes':len(raw),'production_deployed':False,'long_stability_completed':False})
(dest/'drivers').mkdir(exist_ok=True)
for p in local.glob('*.py'):shutil.copy2(p,dest/'drivers'/p.name)
bench=root/'benches/hotpath_bench.rs';shutil.copy2(bench,dest/'hotpath_bench.rs')
review=local/'tokio-quiche-review';(dest/'tokio-quiche-review').mkdir(exist_ok=True)
for name in ['crates-io.json','upstream-files.json']:shutil.copy2(review/name,dest/'tokio-quiche-review'/name)
release=review/'release-0.20.1';selected=['Cargo.toml','src/http3/driver/server.rs','src/http3/driver/hooks.rs','src/http3/driver/mod.rs','src/quic/hooks.rs','src/socket/listener.rs','src/lib.rs']
save(dest/'tokio-quiche-review/release-files.json',{'crate_sha256':sha(review/'tokio-quiche-0.20.1.crate'),'vcs':json.loads((release/'.cargo_vcs_info.json').read_text()),'files':{name:sha(release/name) for name in selected}})
print({'capacity_cases':len(records),'candidate_cases':len(candidate),'raw_hashes':len(raw)})
