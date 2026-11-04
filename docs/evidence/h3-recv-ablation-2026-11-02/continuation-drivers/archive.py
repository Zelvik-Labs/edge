"""Archive all planned cases, including the failed ablation and its strict stop."""
import copy,hashlib,json,re,shutil,subprocess,sys
from pathlib import Path
root=Path.cwd();old=root/'.local/h3-recv-ablation';local=root/'.local/h3-recv-continuation';dest=root/'docs/evidence/h3-recv-ablation-2026-11-02';dest.mkdir(exist_ok=True)
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def save(p,x):p.write_text(json.dumps(x,indent=2,ensure_ascii=False)+'\n')
m=json.loads((old/'metadata.json').read_text())
for n,h in m['source_sha256'].items():assert sha(Path(m['source_snapshot_root'])/n)==h,n
for n,x in m['binaries'].items():assert sha(Path(x['path']))==x['sha256'],n
for key in ['harness_sha256','local_driver_sha256']:
    for n,h in m[key].items():assert sha(root/n)==h,n
continuation=json.loads((local/'continuation-metadata.json').read_text());assert continuation['driver_sha256']==sha(local/'run.py')
save(dest/'metadata.json',m);save(dest/'continuation-metadata.json',continuation)
for pattern in ['test-*.log','clippy.log','release.log','checks*.json','checks.log','*-source.json','batch-removal.patch','measurements.log','postprocess.log']:
    for p in old.glob(pattern):shutil.copy2(p,dest/p.name)
shutil.copy2(local/'run.log',dest/'continuation.log');shutil.copy2(local/'initial-index.json',dest/'initial-stopped-index.json')
index=json.loads((local/'comparison-index.json').read_text());initial=json.loads((local/'initial-index.json').read_text())
assert index['status']=='completed' and len(index['runs'])==18 and index['runs'][:16]==initial['runs']
directory=dest/'comparison';(directory/'manifests').mkdir(parents=True,exist_ok=True)
save(directory/'index.json',index);portable=copy.deepcopy(index);rawdirs=[];runtime=[];failures=[]
for r in portable['runs']:
    p=Path(r['manifest']);x=json.loads(p.read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
    assert m['binaries'][r['label']]['sha256'] in x['binaries'].values()
    for c in x['cases']:
        assert c['fresh_instance'] and c['status'] in ['passed','traffic-errors']
        assert (c['status']=='passed')==(c['native']['failed']==0)
        steals={}
        for i in range(5):
            k=f'cpu{i}';d=[a-b for a,b in zip(c['host_cpu_after'][k],c['host_cpu_before'][k])];steals[k]=d[7]/sum(d[:8])
        row={'round':r['repeat'],'state':r['state'],'label':r['label'],'status':c['status'],'steal':steals,'client_cpu_fraction':c['client_cpu_seconds']/(2*c['elapsed']),'idle_cpu_cores':c['idle_reference']['cpu_cores'],'good_rps':c['native']['good_rps'],'cpu_usec_per_200':c['proxy_cpu_usec_per_200'],'p99_ms':c['native']['p99_ms'],'failed':c['native']['failed']}
        runtime.append(row)
        if c['status']!='passed':
            details={**row,'native':c['native'],'integrity_before':c['integrity_before'],'integrity_after':c['integrity_after'],'origin_requests':c['origin_requests'],'elapsed':c['elapsed'],'target_cpu_throttled_usec_delta':c['proxy_after']['cpu.stat']['throttled_usec']-c['proxy_before']['cpu.stat']['throttled_usec'],'origin_cpu_throttled_usec_delta':c['origin_after']['cpu.stat']['throttled_usec']-c['origin_before']['cpu.stat']['throttled_usec'],'memory_events_after':c['proxy_after']['memory.events'],'empty_retry_warning_count':(p.parent/'edge.log').read_text().count('Upstream body is already finished')}
            failures.append(details)
    target=directory/'manifests'/(p.parent.name+'.json');shutil.copy2(p,target);r['manifest']=str(target.relative_to(directory));rawdirs.append(p.parent)
save(directory/'index.portable.json',portable)
subprocess.run([sys.executable,'scripts/perf/summarize_architecture_candidates.py','--index',str(directory/'index.portable.json'),'--output',str(directory)],check=True)
save(dest/'runtime-quality.json',runtime);save(dest/'failures.json',failures)
functions={};fi=json.loads((local/'functional-index.json').read_text());assert len(fi)==6
save(dest/'functional-index.json',fi)
for r in fi:
    p=Path(r['manifest']);x=json.loads(p.read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
    assert m['binaries'][r['label']]['sha256'] in x['binaries'].values()
    name=f'{r["label"]}-{r["name"]}';(dest/name).mkdir(exist_ok=True);shutil.copy2(p,dest/name/'manifest.json');rawdirs.append(p.parent)
    functions[name]=[{'protocol':c['protocol'],'state':c['cache'],'status':c['status'],'success':c['native']['successful'],'failed':c['native']['failed'],'new_connections':c['native']['new_connections'],'origin_requests':c['origin_requests']} for c in x['cases']]
save(dest/'functional-validation.json',functions)
passed=set();ignored=set()
for p in old.glob('test-*.log'):
    for name,status in re.findall(r'^test (\S+) \.\.\. (ok|ignored)',p.read_text(),re.M):(passed if status=='ok' else ignored).add(name)
save(dest/'test-results.json',{'passed_unique':len(passed),'ignored_unique':len(ignored),'test_names':sorted(passed),'ignored':sorted(ignored)})
raw=[]
for d in rawdirs:
    for p in d.iterdir():
        if p.is_file() and p.name.endswith(('.log','.stderr','.tsv','.tsv.gz','.headers','.command.json')):raw.append(sha(p)+'  '+str(p.relative_to(root)))
(dest/'raw-files.sha256').write_text('\n'.join(sorted(raw))+'\n')
for name,source in [('initial-drivers',old),('continuation-drivers',local)]:
    (dest/name).mkdir(exist_ok=True)
    for p in source.glob('*.py'):shutil.copy2(p,dest/name/p.name)
save(dest/'continuation-tools.json',{str(p.relative_to(root)):sha(p) for p in local.glob('*.py')})
print({'capacity_cases':len(index['runs']),'capacity_failed':len(failures),'capacity_native_errors':sum(x['failed'] for x in failures),'functional_cases':sum(len(v) for v in functions.values()),'functional_errors':sum(c['failed'] for v in functions.values() for c in v),'raw_hashes':len(raw)})
