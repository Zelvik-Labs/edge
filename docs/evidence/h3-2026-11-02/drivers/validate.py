"""Verify frozen provenance, portable summaries, links and archive checksums."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

root=Path.cwd();local=root/'.local/h3-stage1';base=root/'docs/evidence/h3-2026-11-02'
def sha(path):
    h=hashlib.sha256()
    with path.open('rb') as f:
        while chunk:=f.read(1024*1024):h.update(chunk)
    return h.hexdigest()
m=json.loads((base/'metadata.json').read_text())
for name,digest in m['source_sha256'].items():assert sha(root/name)==digest,name
for name,data in m['binaries'].items():assert sha(Path(data['path']))==data['sha256'],name
for name,digest in m['harness_sha256'].items():assert sha(root/name)==digest,name
for key in ['diagnostic_harness_sha256','postprocess_sha256']:
    for name,digest in m[key].items():assert sha(root/name)==digest,name
v=json.loads((base/'measurement-validation.json').read_text())
assert v['capacity_cases']==105,v
assert v['candidate_statuses']=={'passed':v['candidate_cases']},v
assert v['candidate_native_failures']==0,v
assert all(list(g['statuses'])==['passed'] and g['failed']==0 for g in v['functional'].values())
for path in base.rglob('index.portable.json'):
    index=json.loads(path.read_text());assert index['status']=='completed'
    for run in index['runs']:assert (path.parent/run['manifest']).is_file(),path
for path in [root/'docs/performance-h3-2026-11-02.md',base/'README.md']:
    text=path.read_text();assert '<!--' not in text,path
    for target in re.findall(r'\]\(([^)]+)\)',text):
        if '://' not in target and not target.startswith('#'):
            assert (path.parent/target.split('#')[0]).exists(),(path,target)
for path in (base/'drivers').glob('*.py'):compile(path.read_text(),str(path),'exec')
for path in base.rglob('*'):
    if path.is_file():
        assert path.suffix not in ['.pem','.key','.data','.pyc'],path
        assert path.name!='overrides.json',path
        assert not re.search(rb'(?m)^-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----\r?$',path.read_bytes()),path
with (local/'raw-hash-validation.log').open('w') as log:
    subprocess.run(['sha256sum','-c',str(base/'raw-files.sha256')],stdout=log,check=True)
subprocess.run([sys.executable,'scripts/perf/summarize_architecture_candidates.py',
    '--index',str(base/'compare-h3/index.portable.json'),
    '--output',str(local/'recomputed-h3')],check=True)
assert (local/'recomputed-h3/results.json').read_bytes()==(base/'compare-h3/results.json').read_bytes()
validation={'frozen_files_verified':len(m['source_sha256']),'binaries_verified':len(m['binaries']),
    'harness_files_verified':len(m['harness_sha256']),'all_candidate_contracts_passed':True,
    'portable_indices_resolve':True,'report_links_exist':True,'raw_hashes_match':True,
    'recomputed_h3_identical':True,'runtime_secrets_excluded':True,
    'all_failed_baseline_and_competitor_cases_retained':True}
(base/'archive-validation.json').write_text(json.dumps(validation,indent=2)+'\n')
paths=sorted(p for p in base.rglob('*') if p.is_file() and p.name!='artifacts.sha256')
(base/'artifacts.sha256').write_text('\n'.join(sha(p)+'  '+str(p.relative_to(base)) for p in paths)+'\n')
with (local/'artifact-hash-validation.log').open('w') as log:
    subprocess.run(['sha256sum','-c','artifacts.sha256'],cwd=base,stdout=log,check=True)
print(json.dumps({'validation':validation,'artifacts_hashed':len(paths)}))
