"""Build and test the one-packet variant; reuse the primary target explicitly."""
import hashlib,json,os,subprocess
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-recv-ablation';env={**os.environ,'CARGO_TARGET_DIR':'/workspace/edge/target'}
sources=json.loads((local/'candidate-source.json').read_text())
for n,h in sources.items():assert hashlib.sha256((root/n).read_bytes()).hexdigest()==h,n
jobs=[('test-quic',['test','--locked','--lib','quic_udp_demux::']),('test-governor',['test','--locked','--lib','memory_governor::']),('test-h3',['test','--locked','--lib','h3']),('test-cache',['test','--locked','--lib','cache']),('test-proxy',['test','--locked','--lib','proxy::tests']),('test-l4',['test','--locked','--lib','l4_defense::']),('clippy',['clippy','--locked','--lib','--tests','--','-D','clippy::correctness','-D','clippy::suspicious']),('release',['build','--locked','--release','--bin','Zelvik-Edge'])]
results=[];code=0
for name,args in jobs:
    cmd=['/workspace/docs/scripts/workspace/local-rust.sh',*args]
    with (local/(name+'.log')).open('x') as log:code=subprocess.run(cmd,env=env,stdout=log,stderr=subprocess.STDOUT).returncode
    results.append({'name':name,'command':cmd,'returncode':code});(local/'checks.json').write_text(json.dumps(results,indent=2)+'\n');print(name,code,flush=True)
    if code:break
(local/'checks-completed.json').write_text(json.dumps({'returncode':code})+'\n')
raise SystemExit(code)
