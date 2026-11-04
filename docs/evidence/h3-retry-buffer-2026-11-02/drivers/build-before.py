"""Wait for frozen measurements, restore tested receive batching, build diagnostic baseline."""
import hashlib,json,os,shutil,subprocess,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-retry-buffer';local.mkdir(exist_ok=True)
continuation=root/'.local/h3-recv-continuation';deadline=time.monotonic()+30*60
while time.monotonic()<deadline:
    log=(continuation/'run.log').read_text()
    if 'Continuation complete; original failure retained' in log:break
    if 'Traceback (most recent call last)' in log:raise RuntimeError('Continuation failed')
    time.sleep(5)
else:raise TimeoutError('Continuation incomplete')
fi=json.loads((continuation/'functional-index.json').read_text());assert len(fi)==6 and all(all(s=='passed' for s in r['case_statuses']) for r in fi)
index=json.loads((continuation/'comparison-index.json').read_text());assert index['status']=='completed' and len(index['runs'])==18
assert all(r['case_statuses']==['passed'] for r in index['runs'] if r['label']=='batched')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
ablation=json.loads((root/'.local/h3-recv-ablation/metadata.json').read_text())
for n,h in ablation['source_sha256'].items():assert sha(root/n)==h,n
old=json.loads((root/'.local/h3-stage2/build-metadata.json').read_text())
name='src/quic_udp_demux.rs';(root/name).write_bytes((Path(old['source_snapshot_root'])/name).read_bytes())
for n,h in old['source_sha256'].items():assert sha(root/n)==h,n
before=local/'before';before.mkdir(exist_ok=False);snapshot=before/'source'
for n in old['source_sha256']:
    p=snapshot/n;p.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(root/n,p)
meta={'source_sha256':old['source_sha256'],'source_snapshot_root':str(snapshot),'release_reference_sha256':old['binaries']['candidate']['sha256'],'profile':{'name':'dev','opt_level':2,'codegen_units':16,'lto':False,'debug':0,'target_cpu':'x86-64-v2'},'reason':'functional diagnostic of empty retry buffer; no release capacity comparison','restored_batching_only_after_measurements':True}
(before/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
cmd=['/workspace/docs/scripts/workspace/local-rust.sh','build','--locked','--bin','Zelvik-Edge']
with (before/'build.log').open('x') as log:code=subprocess.run(cmd,env={**os.environ,'CARGO_TARGET_DIR':'/workspace/edge/target'},stdout=log,stderr=subprocess.STDOUT).returncode
meta['build_returncode']=code
if code==0:
    binary=before/'Zelvik-Edge';shutil.copy2('/workspace/edge/target/debug/Zelvik-Edge',binary);meta['binary']={'path':str(binary),'sha256':sha(binary)}
    for n,h in meta['source_sha256'].items():assert sha(root/n)==h,n
(before/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
print('Diagnostic baseline build complete',code,flush=True)
raise SystemExit(code)
