"""Measure completed intended artifacts after cancelling extra dev builds."""
import hashlib,json,os,subprocess,sys
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2'
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
checks=json.loads((local/'check-jobs.json').read_text())
for name in ['test-memory_governor','test-l4_defense','test-quic_udp_demux','test-h3','test-cache','test-proxy','clippy','release-build-retry']:
    assert any(j['name']==name and j['returncode']==0 for j in checks),name
assert checks[-1]['name']=='microbench-build' and checks[-1]['returncode']==130
cancel=json.loads((local/'supplemental-build-cancellation.json').read_text())
artifact=json.loads((local/'microbench-artifact-ready.json').read_text());bench=Path(artifact['executable'])
assert sha(bench)==artifact['sha256']
for name,digest in json.loads((local/'candidate-source.json').read_text()).items():assert sha(root/name)==digest,name
assert sha(Path('/workspace/edge/target/release/Zelvik-Edge'))=='8226346bcba3eb8609eba60a4fe61308b10196643ac6d1734e09a00d6c492966'
for p in Path('/proc').iterdir():
    if not p.name.isdigit():continue
    try:a=(p/'cmdline').read_bytes().split(b'\0')
    except (FileNotFoundError,PermissionError,ProcessLookupError):continue
    assert not (a and a[0].endswith(b'/rustc') and b'--crate-name' in a),p
(local/'measurement-readiness.json').write_text(json.dumps({'required_checks_passed':True,'intended_benchmark_artifact_completed':True,'supplemental_cargo_build_cancelled':True,'supplemental_cargo_returncode':130,'benchmark_sha256':artifact['sha256'],'no_active_rust_compilers':True},indent=2)+'\n')
with (local/'microbench.log').open('x') as log:
    subprocess.run(['taskset','-c','0,1',str(bench),'h3_governor_reads','--bench','--sample-size','30','--measurement-time','3','--warm-up-time','1'],env={**os.environ,'CRITERION_HOME':str(local/'criterion')},stdout=log,stderr=subprocess.STDOUT,check=True)
print('Microbenchmark completed',flush=True)
subprocess.run([sys.executable,str(local/'run-suite.py')],check=True)
subprocess.run([sys.executable,str(local/'profile-h3.py')],check=True)
print('H3 stage 2 measurements complete',flush=True)
