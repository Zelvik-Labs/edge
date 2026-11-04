"""Archive manifests and summaries; leave runtime secrets and raw traces local."""
from collections import Counter
import copy
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

root = Path.cwd()
local = root / '.local/h3-stage1'
dest = root / 'docs/evidence/h3-2026-11-02'
dest.mkdir(exist_ok=True, parents=True)
def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        while data := f.read(1024 * 1024): h.update(data)
    return h.hexdigest()
def save(path, data): path.write_text(json.dumps(data, indent=2, ensure_ascii=False)+'\n')
metadata = json.loads((local/'build-metadata.json').read_text())
for name, digest in metadata['source_sha256'].items(): assert sha(root/name) == digest, name
for name, value in metadata['binaries'].items(): assert sha(Path(value['path'])) == value['sha256'], name
for name, digest in metadata['harness_sha256'].items(): assert sha(root/name) == digest, name
metadata['diagnostic_harness_sha256']={str(path.relative_to(root)):sha(path) for path in [
    root/'scripts/perf/profile_architecture.py',root/'scripts/perf/summarize_architecture.py',
    local/'profile-h3.py',local/'after-suite.py']}
metadata['postprocess_sha256']={str((local/'summarize-h3-profiles.py').relative_to(root)):
    sha(local/'summarize-h3-profiles.py')}
save(dest/'metadata.json', metadata)
for pattern in ['tests-*.log', 'test-results.json', 'clippy*.log', 'python-tests.log',
                'release-build*.log', 'storage-*.json', '*gc*.json', 'cleanup-*.log',
                'candidate-source.json', 'baseline-source.json', 'h3.patch', 'suite-runs.json',
                'profiles-after-summary.json']:
    for path in local.glob(pattern): shutil.copy2(path, dest/path.name)

records = []
raw_directories = []
for name in ['compare-h3', 'compare-fresh', 'compare-32k']:
    source = local/name/'index.json'
    index = json.loads(source.read_text())
    assert index['status'] == 'completed'
    directory = dest/name
    (directory/'manifests').mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, directory/'index.json')
    portable = copy.deepcopy(index)
    portable['original_index_sha256'] = sha(source)
    for run in portable['runs']:
        path = Path(run['manifest'])
        manifest = json.loads(path.read_text())
        assert manifest['status'] == 'completed' and not manifest.get('cleanup_errors')
        assert all(c.get('fresh_instance') for c in manifest['cases'])
        assert manifest['driver_sha256'] == metadata['harness_sha256']['scripts/perf/protocol_state_matrix.py']
        if run['label'] in ['baseline','candidate']:
            assert metadata['binaries'][run['label']]['sha256'] in manifest['binaries'].values()
        target = directory/'manifests'/(path.parent.name+'.json')
        shutil.copy2(path, target)
        run['manifest'] = str(target.relative_to(directory))
        records.extend(dict(case, label=run['label'], round=run['repeat'], group=name) for case in manifest['cases'])
        raw_directories.append(path.parent)
    save(directory/'index.portable.json', portable)
    subprocess.run([sys.executable, 'scripts/perf/summarize_architecture_candidates.py',
        '--index', str(directory/'index.portable.json'), '--output', str(directory)], check=True)

functional = {}
for name in ['candidate-fresh-long','candidate-h1-h2','smoke','functional-128k']:
    manifest = json.loads((local/name/'manifest.json').read_text())
    assert manifest['status'] == 'completed' and not manifest.get('cleanup_errors')
    assert metadata['binaries']['candidate']['sha256'] in manifest['binaries'].values()
    directory = dest/name
    directory.mkdir(exist_ok=True)
    shutil.copy2(local/name/'manifest.json', directory/'manifest.json')
    if name in ['smoke','functional-128k']:
        functional[name] = {'statuses': dict(Counter(c['status'] for c in manifest['cases'])),
            'successful': sum(c.get('native',{}).get('successful',0) for c in manifest['cases']),
            'failed': sum(c.get('native',{}).get('failed',0) for c in manifest['cases'])}
    else:
        records.extend(dict(case, label='candidate', round=0, group=name) for case in manifest['cases'])
        save(directory/'index.portable.json', {'status':'completed','runs':[
            {'label':'candidate','repeat':0,'manifest':'manifest.json'}]})
        subprocess.run([sys.executable, 'scripts/perf/summarize_architecture_candidates.py',
            '--index',str(directory/'index.portable.json'),'--output',str(directory)],check=True)
    raw_directories.append(local/name)

diagnostics = {}
for name in ['baseline','fresh-diagnostic','fresh-strace','fresh-direct','fresh-endpoint',
             'fresh-server-strace','fresh-demux-baseline']:
    path = local/name/'manifest.json'
    manifest = json.loads(path.read_text())
    assert manifest['status'] == 'completed' and not manifest.get('cleanup_errors')
    directory = dest/'diagnostics'/name
    directory.mkdir(parents=True,exist_ok=True)
    shutil.copy2(path,directory/'manifest.json')
    diagnostics[name] = {'diagnostic_only':True,
        'statuses':dict(Counter(c['status'] for c in manifest['cases']))}
    raw_directories.append(path.parent)
    if name == 'fresh-server-strace':
        for filename in ['udp-before.txt','udp-after.txt','strace-attach.log']:
            shutil.copy2(path.parent/filename,directory/filename)
    if name == 'fresh-demux-baseline':
        lines=(path.parent/'edge.log').read_text().splitlines()
        hits=[line for line in lines if 'new route rate limit reached' in line or 'pending route rate limit reached' in line]
        save(directory/'route-window-rejections.json', {'messages':len(hits),
            'first_messages':hits[:4],'normal_per_ip_routes':128,'window_seconds':10,
            'full_log_sha256':sha(path.parent/'edge.log')})

profile_manifest=json.loads((local/'profiles-after/manifest.json').read_text())
assert profile_manifest['status']=='completed' and not profile_manifest.get('cleanup_errors')
assert metadata['binaries']['candidate']['sha256'] in profile_manifest['binaries'].values()
assert all(c['status']=='passed' and c['native']['failed']==0 for c in profile_manifest['cases'])
(dest/'profiles-after').mkdir(exist_ok=True)
shutil.copy2(local/'profiles-after/manifest.json',dest/'profiles-after/manifest.json')
for path in (local/'profiles-after').glob('*.perf-flat.txt'):
    shutil.copy2(path,dest/'profiles-after'/path.name)
raw_directories.append(local/'profiles-after')

raw = []
for directory in raw_directories:
    for path in sorted(directory.iterdir()):
        if path.is_file() and (path.name.endswith(('.log','.stderr','.tsv','.tsv.gz','.headers','.command.json','.stacks.txt'))
                or path.name.startswith('server-strace.')):
            raw.append(sha(path)+'  '+str(path.relative_to(root)))
for path in sorted(local.glob('client-strace.*.log')):
    raw.append(sha(path)+'  '+str(path.relative_to(root)))
path=local/'profiles-after-summary-original.json'
raw.append(sha(path)+'  '+str(path.relative_to(root)))
(dest/'raw-files.sha256').write_text('\n'.join(raw)+'\n')
candidate=[c for c in records if c['label']=='candidate']
save(dest/'measurement-validation.json', {
    'capacity_cases':len(records),'capacity_statuses':dict(Counter(c['status'] for c in records)),
    'candidate_cases':len(candidate),'candidate_statuses':dict(Counter(c['status'] for c in candidate)),
    'known_native_failures':sum(c.get('native',{}).get('failed',0) for c in records),
    'candidate_native_failures':sum(c.get('native',{}).get('failed',0) for c in candidate),
    'cases_without_native_result':sum('native' not in c for c in records),
    'functional':functional,'diagnostics':diagnostics,'raw_hashes':len(raw),
    'profiles':{'diagnostic_only':True,'cases':len(profile_manifest['cases']),
                'failed_transfers':0},
    'all_frozen_hashes_verified':True,'all_failures_retained':True,
    'production_deployed':False,'long_stability_completed':False})
(dest/'drivers').mkdir(exist_ok=True)
for path in local.glob('*.py'):
    if path.name != 'archive.py': shutil.copy2(path,dest/'drivers'/path.name)
shutil.copy2(local/'archive.py',dest/'drivers/archive.py')
for name in ['diagnostic-client.c','direct-client.c']:
    shutil.copy2(local/name,dest/'drivers'/name)
print(json.dumps({'capacity_cases':len(records),'candidate_cases':len(candidate),
                 'raw_hashes':len(raw),'functional':functional}))
