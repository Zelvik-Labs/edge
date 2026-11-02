"""Check the two-file fix and validate it with the matching diagnostic profile."""
import difflib,hashlib,json,os,re,shutil,subprocess,sys
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-retry-buffer';after=local/'after';after.mkdir(exist_ok=False)
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
before=json.loads((local/'before/metadata.json').read_text());red=json.loads((local/'red-result.json').read_text())
assert red['returncode']==101 and len(red['failed'])==2 and len(red['passed'])==1
sources={n:sha(root/n) for n in before['source_sha256']}
assert {n for n,h in sources.items() if before['source_sha256'][n]!=h}=={'src/h3_downstream.rs','src/proxy.rs'}
snapshot=after/'source'
for n in sources:
    p=snapshot/n;p.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(root/n,p)
patch=[]
for n in ['src/h3_downstream.rs','src/proxy.rs']:
    patch.extend(difflib.unified_diff((Path(before['source_snapshot_root'])/n).read_text().splitlines(True),(root/n).read_text().splitlines(True),fromfile='before/'+n,tofile='after/'+n))
(local/'implementation.patch').write_text(''.join(patch))
m={'source_sha256':sources,'source_snapshot_root':str(snapshot),'profile':before['profile'],'before_binary_sha256':before['binary']['sha256'],'capacity_comparison':False,'release_capacity_for_this_fix_completed':False,'only_changed_source':['src/h3_downstream.rs','src/proxy.rs'],'driver_sha256':sha(Path(__file__))}
def save():(after/'metadata.json').write_text(json.dumps(m,indent=2)+'\n')
save();env={**os.environ,'CARGO_TARGET_DIR':'/workspace/edge/target'}
jobs=[('test-quic',['test','--locked','--lib','quic_udp_demux::']),('test-governor',['test','--locked','--lib','memory_governor::']),('test-h3',['test','--locked','--lib','h3']),('test-cache',['test','--locked','--lib','cache']),('test-proxy',['test','--locked','--lib','proxy::tests']),('test-l4',['test','--locked','--lib','l4_defense::']),('clippy',['clippy','--locked','--lib','--tests','--','-D','clippy::correctness','-D','clippy::suspicious']),('build',['build','--locked','--bin','Zelvik-Edge'])]
checks=[]
for name,args in jobs:
    cmd=['/workspace/docs/scripts/workspace/local-rust.sh',*args]
    with (after/(name+'.log')).open('x') as log:code=subprocess.run(cmd,env=env,stdout=log,stderr=subprocess.STDOUT).returncode
    checks.append({'name':name,'command':cmd,'returncode':code});(after/'checks.json').write_text(json.dumps(checks,indent=2)+'\n');print(name,code,flush=True)
    assert code==0,(name,code)
    for n,h in sources.items():assert sha(root/n)==h,n
binary=after/'Zelvik-Edge';shutil.copy2('/workspace/edge/target/debug/Zelvik-Edge',binary);m['binary']={'path':str(binary),'sha256':sha(binary)};save()
passed=set();ignored=set()
for p in after.glob('test-*.log'):
    for name,status in re.findall(r'^test (\S+) \.\.\. (ok|ignored)',p.read_text(),re.M):(passed if status=='ok' else ignored).add(name)
(after/'test-results.json').write_text(json.dumps({'passed_unique':len(passed),'ignored_unique':len(ignored),'test_names':sorted(passed),'ignored':sorted(ignored)},indent=2)+'\n')
a=json.loads((root/'.local/h3-recv-ablation/metadata.json').read_text())
common=['--target','edge','--tls',str(root/'.local/perf-next/tls'),'--client',a['binaries']['client']['path'],'--controller',a['binaries']['controller']['path'],'--edge-binary',str(binary),'--protocols','h3','--repeats','1','--concurrency','32','--idle-reference-seconds','2']
functions=[('probe-off',['--states','off','--connections','reuse','--seconds','3']),('cache',['--states','hit','miss','--connections','reuse','--seconds','3','--cold-requests','128']),('fresh',['--states','hit','off','--connections','fresh','--fresh-requests','2048','--seconds','10'])]
results=[]
for name,flags in functions:
    directory=after/name;cmd=[sys.executable,'scripts/perf/protocol_state_matrix.py',*common,'--output',str(directory),*flags]
    with (after/(name+'.log')).open('x') as log:code=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT).returncode
    x=json.loads((directory/'manifest.json').read_text());assert x['status']=='completed' and not x.get('cleanup_errors')
    r={'name':name,'command':cmd,'manifest':str(directory/'manifest.json'),'returncode':code,'case_statuses':[c['status'] for c in x['cases']],'native_errors':sum(c['native']['failed'] for c in x['cases']),'last_case_empty_final_body_warnings':(directory/'edge.log').read_text().count('Upstream body is already finished')}
    results.append(r);(after/'functional-index.json').write_text(json.dumps(results,indent=2)+'\n')
    assert code==0 and all(s=='passed' for s in r['case_statuses']) and r['native_errors']==0,r
    assert r['last_case_empty_final_body_warnings']==0,r
    assert m['binary']['sha256'] in x['binaries'].values()
    print(name,r['case_statuses'],'warnings',r['last_case_empty_final_body_warnings'],flush=True)
for n,h in sources.items():assert sha(root/n)==h,n
(after/'completed.json').write_text(json.dumps({'returncode':0,'unit_tests':len(passed),'ignored':len(ignored),'functional_cases':sum(len(r['case_statuses']) for r in results),'warning_count':0})+'\n')
print('Retry buffer fix validated',len(passed),'unit tests; 5 functional cases; zero target warnings',flush=True)
