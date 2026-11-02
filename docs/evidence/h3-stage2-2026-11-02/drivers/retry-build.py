"""Reuse the original primary release target after the budget abort."""
import hashlib,json,os,subprocess
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';env={**os.environ,'CARGO_TARGET_DIR':'/workspace/edge/target'}
sources=json.loads((local/'candidate-source.json').read_text())
for name,digest in sources.items():assert hashlib.sha256((root/name).read_bytes()).hexdigest()==digest,name
results=json.loads((local/'check-jobs.json').read_text())
code=0
for name,args in [('release-build-retry',['build','--locked','--release','--bin','Zelvik-Edge']),('microbench-build',['build','--locked','--profile','dev','--bench','hotpath_bench','--message-format=json'])]:
    command=['/workspace/docs/scripts/workspace/local-rust.sh',*args]
    with (local/(name+'.log')).open('x') as log:code=subprocess.run(command,env=env,stdout=log,stderr=subprocess.STDOUT).returncode
    results.append({'name':name,'command':command,'cargo_target_dir':env['CARGO_TARGET_DIR'],'returncode':code});(local/'check-jobs.json').write_text(json.dumps(results,indent=2)+'\n');print(name,code,flush=True)
    if code:break
(local/'checks-completed.json').write_text(json.dumps({'returncode':code})+'\n')
raise SystemExit(code)
