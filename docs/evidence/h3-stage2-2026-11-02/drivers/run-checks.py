"""Run serialized checks, the release and the diagnostic microbench build."""
import json
from pathlib import Path
import subprocess

root=Path.cwd();local=root/'.local/h3-stage2'
wrapper='/workspace/docs/scripts/workspace/local-rust.sh'
jobs=[('test-memory_governor',['test','--locked','--lib','memory_governor::']),
      ('test-l4_defense',['test','--locked','--lib','l4_defense::']),
      ('test-quic_udp_demux',['test','--locked','--lib','quic_udp_demux::']),
      ('test-h3',['test','--locked','--lib','h3']),
      ('test-cache',['test','--locked','--lib','cache']),
      ('test-proxy',['test','--locked','--lib','proxy::tests']),
      ('clippy',['clippy','--locked','--lib','--tests','--bench','hotpath_bench','--','-D','clippy::correctness','-D','clippy::suspicious']),
      ('release-build',['build','--locked','--release','--bin','Zelvik-Edge']),
      ('microbench-build',['build','--locked','--profile','dev','--bench','hotpath_bench','--message-format=json'])]
results=[]
for name,args in jobs:
    command=[wrapper,*args]
    with (local/(name+'.log')).open('x') as log:
        code=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT).returncode
    results.append({'name':name,'command':command,'returncode':code})
    (local/'check-jobs.json').write_text(json.dumps(results,indent=2)+'\n')
    print(name,code,flush=True)
    if code:raise SystemExit(code)
