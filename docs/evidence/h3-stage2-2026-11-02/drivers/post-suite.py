"""Run policy tests and evidence processing only after all measurements stop."""
import hashlib,json,os,subprocess,sys,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';deadline=time.monotonic()+90*60
while time.monotonic()<deadline:
    text=(local/'measurements-final.log').read_text()
    if 'H3 stage 2 measurements complete' in text:break
    if 'Traceback (most recent call last)' in text:raise RuntimeError('Measurement failed; inspect before postprocessing')
    time.sleep(5)
else:raise TimeoutError('Measurements did not complete')
subprocess.run([sys.executable,str(local/'recheck-reuse.py')],check=True)
with (local/'test-cloud-cache-policy.log').open('x') as log:
    subprocess.run([sys.executable,'-I','/workspace/docs/scripts/workspace/test-local-storage.py'],env={**os.environ,'ZELVIK_LOCAL_STORAGE_PROFILE':'local'},stdout=log,stderr=subprocess.STDOUT,check=True)
with (local/'cloud-cache-profile.json').open('w') as out:
    subprocess.run([sys.executable,'-I','/workspace/docs/scripts/workspace/local-storage.py','check','--json'],stdout=out,check=True)
s=json.loads((local/'cloud-cache-profile.json').read_text());assert s['ok'] and s['budget_profile']=='cloud'
assert s['categories']['mbx']['budget_bytes']>12*(1<<30) and s['min_free_bytes']==5*(1<<30)
with (local/'cloud-mbx-gc-dry-run.json').open('w') as out:subprocess.run(['mbx','gc','--dry-run','--json'],stdout=out,check=True)
gc=json.loads((local/'cloud-mbx-gc-dry-run.json').read_text())
for key in ['max_bytes','target_max_bytes','incremental_max_bytes','max_total_bytes']:assert gc[key]==32*(1<<30),(key,gc[key])
paths=['scripts/workspace/local-storage.py','scripts/workspace/test-local-storage.py','docs/operations/workspace.md','docs/operations/agent-rules.md']
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
with (local/'cloud-cache-policy.patch').open('w') as out:subprocess.run(['git','diff','--',*paths],cwd='/workspace/docs',stdout=out,check=True)
config={'source_sha256':{str(Path('/workspace/docs')/p):sha(Path('/workspace/docs')/p) for p in paths},'authorized_by_user':True,'applied_after_release_build':True,'changed_during_capacity':True,'benchmark_server_resource_limits_unchanged':True,'environment':{key:os.environ.get(key) for key in ['ZELVIK_LOCAL_STORAGE_PROFILE','MBX_GC_AUTO','MBX_GC_MAX_SIZE','MBX_TARGET_MAX_SIZE','MBX_GC_INCREMENTAL_MAX_SIZE','MBX_GC_MAX_TOTAL_SIZE','MBX_LEARNED_INCREMENTAL_MAX_SIZE']},'storage_report_sha256':sha(local/'cloud-cache-profile.json'),'dry_run_only':True}
(local/'cloud-cache-policy.json').write_text(json.dumps(config,indent=2)+'\n')
for script in ['summarize-profiles.py','record-extra-provenance.py','archive.py','render-results.py','finish-report.py']:
    subprocess.run([sys.executable,str(local/script)],check=True)
print('Report generated; manual conclusions review and final validation remain',flush=True)
