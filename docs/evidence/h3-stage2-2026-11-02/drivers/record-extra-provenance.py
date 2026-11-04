"""Attach built microbenchmark and diagnostic-driver provenance after runs."""
import hashlib,json
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2'
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
p=local/'build-metadata.json';m=json.loads(p.read_text())
source_root=Path(m.get('source_snapshot_root',root))
for name,digest in m['source_sha256'].items():assert sha(source_root/name)==digest,name
for name,d in m['binaries'].items():assert sha(Path(d['path']))==d['sha256'],name
bench=[]
for line in (local/'microbench-build.log').read_text().splitlines():
    try:item=json.loads(line)
    except json.JSONDecodeError:continue
    if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='hotpath_bench' and item.get('executable'):bench.append(Path(item['executable']))
assert len(bench)==1,bench
previous=m.get('microbenchmark',{})
bench_path=Path(previous.get('frozen_path',bench[0]))
m['microbenchmark']={**previous,'path':str(bench_path),'measured_path':str(bench[0]),'sha256':sha(bench_path),'profile':'dev','opt_level':2,'codegen_units':16,'lto':False,'capacity_excluded':True}
m['diagnostic_harness_sha256']={str(path.relative_to(root)):sha(path) for path in [root/'scripts/perf/profile_architecture.py',root/'scripts/perf/summarize_architecture.py']}
m['local_driver_sha256']={str(path.relative_to(root)):sha(path) for path in sorted(local.glob('*.py'))}
m['cargo_target_dir']='/workspace/edge/target';m['checks']=json.loads((local/'check-jobs.json').read_text())
m['measurement_readiness']=json.loads((local/'measurement-readiness.json').read_text())
m['cloud_cache_policy']=json.loads((local/'cloud-cache-policy.json').read_text())
for phase,binary in [('before','baseline'),('after','candidate')]:
    manifest=json.loads((local/('profiles-'+phase)/'manifest.json').read_text())
    assert manifest['status']=='completed' and not manifest.get('cleanup_errors')
    assert m['binaries'][binary]['sha256'] in manifest['binaries'].values()
    assert all(c['status']=='passed' and c['native']['failed']==0 for c in manifest['cases'])
p.write_text(json.dumps(m,indent=2)+'\n')
print({'microbenchmark_sha256':m['microbenchmark']['sha256'],'local_drivers':len(m['local_driver_sha256'])})
