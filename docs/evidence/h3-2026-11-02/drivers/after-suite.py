"""Run CPU diagnosis after all uninstrumented capacity cases have exited."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import time

local=Path('.local/h3-stage1')
deadline=time.monotonic()+60*60
while time.monotonic()<deadline:
    log=(local/'suite.log').read_text()
    if 'Traceback (most recent call last)' in log:
        raise RuntimeError('Suite failed; inspect candidate before profiling')
    if 'H3 suite complete' in log:break
    time.sleep(5)
else:raise TimeoutError('Capacity suite did not complete')
with (local/'profiles-after.log').open('x') as log:
    subprocess.run([sys.executable,str(local/'profile-h3.py')],stdout=log,
                   stderr=subprocess.STDOUT,check=True)
spec=importlib.util.spec_from_file_location('summary','scripts/perf/summarize_architecture.py')
summary=importlib.util.module_from_spec(spec);spec.loader.exec_module(summary)
summary.GROUPS.update({'quic_demux':'::quic_udp_demux::','quinn_io':'quinn::',
    'quinn_protocol':'quinn_proto::','h3_protocol':'h3::','h3_adapter':'::h3_downstream::',
    'mpsc':'tokio::sync::mpsc','governor_snapshot':'MemoryGovernor>::snapshot'})
manifest=json.loads((local/'profiles-after/manifest.json').read_text())
assert manifest['status']=='completed' and not manifest.get('cleanup_errors')
profiles=[]
for case in manifest['cases']:
    assert case['status']=='passed' and case['native']['failed']==0
    p=summary.profile(local/'profiles-after'/(case['stem']+'.stacks.txt'))
    p['state']=case['cache'];profiles.append(p)
(local/'profiles-after-summary.json').write_text(json.dumps({
    'diagnostic_rps_excluded':True,'inclusive_groups_overlap':True,'profiles':profiles},indent=2)+'\n')
print('H3 CPU profiles complete')
