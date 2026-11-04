"""Archive bounded, inspectable evidence without credentials or raw stack memory."""
from collections import Counter
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

root = Path.cwd()
local = root / '.local/architecture-stage2'
evidence = root / 'docs/evidence/architecture-stage2-2026-11-02'
evidence.mkdir(parents=True, exist_ok=True)

def sha(path):
    result = hashlib.sha256()
    with path.open('rb') as file:
        while chunk := file.read(1024 * 1024):
            result.update(chunk)
    return result.hexdigest()

def save(path, data):
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + '\n')

metadata = json.loads((local / 'build-metadata.json').read_text())
for path, expected in metadata['source_sha256'].items():
    assert sha(root / path) == expected, path
for label, data in metadata['binaries'].items():
    assert sha(Path(data['path'])) == data['sha256'], label
metadata['harness_source_sha256'] = {
    str(p.relative_to(root)): sha(p) for p in [
        *sorted((root / 'scripts/perf').glob('*.py')),
        root / 'benches/support/protocol_state_client.c',
        root / 'examples/issue34-controller.rs']}
save(evidence / 'metadata.json', metadata)
for pattern in ['test-*.log', 'test-results.json', 'tests-build*.log', 'clippy.log',
                'python-tests.log', 'candidate-release*.log', 'candidate-source.json',
                'baseline-source.json', 'changed-source.json', 'stage2.patch',
                'storage-*.json', 'gc-applied.json', 'profiles-*-summary.json',
                'comparison-runs.json', 'followup-runs.json', 'fixture-warnings.json',
                'runtime-image-observation.json']:
    for path in local.glob(pattern):
        shutil.copy2(path, evidence / path.name)

records, raw_dirs, groups = [], [], {}
for source, destination in [
    ('comparison-h1', 'h1'), ('comparison-tls-h2', 'tls-h2'),
    ('comparison-fresh', 'fresh'), ('comparison-cold-long', 'cold-long'),
    ('comparison-32k', '32k'), ('regression-recheck', 'regression-recheck'),
    ('comparison-uniform-tls', 'uniform-tls')]:
    index_path = local / source / 'index.json'
    index = json.loads(index_path.read_text())
    assert index['status'] == 'completed', source
    dest = evidence / destination
    (dest / 'manifests').mkdir(parents=True, exist_ok=True)
    shutil.copy2(index_path, dest / 'index.json')
    portable = copy.deepcopy(index)
    portable['original_index_sha256'] = sha(index_path)
    for entry in portable['runs']:
        path = Path(entry['manifest'])
        manifest = json.loads(path.read_text())
        assert manifest['status'] == 'completed' and not manifest.get('cleanup_errors')
        assert manifest['driver_sha256'] == sha(root / 'scripts/perf/protocol_state_matrix.py')
        if entry['label'] in {'baseline', 'candidate'}:
            assert metadata['binaries'][entry['label']]['sha256'] in manifest['binaries'].values()
        for case in manifest['cases']:
            assert case['fresh_instance']
            count = case.get('native', {}).get('successful', 0)
            cpu = {name: ((case['proxy_after']['cpu.stat'][name] - case['proxy_before']['cpu.stat'][name]) / count)
                   if count and case.get('proxy_after') else None
                   for name in ['usage_usec', 'user_usec', 'system_usec']}
            records.append({'label': entry['label'], 'status': case['status'],
                            'failed': case.get('native', {}).get('failed'),
                            'group': destination, 'round': entry['repeat'],
                            'protocol': case['protocol'], 'connection': case['connection'],
                            'cache': case['cache'], 'successful': count, 'cpu_usec_per_200': cpu})
        name = path.parent.name + '.json'
        shutil.copy2(path, dest / 'manifests' / name)
        if destination == 'uniform-tls':
            probe_dest = dest / 'negotiation' / path.parent.name
            probe_dest.mkdir(parents=True, exist_ok=True)
            for probe in path.parent.glob('*.tls-negotiation.json'):
                shutil.copy2(probe, probe_dest / probe.name)
        entry['manifest'] = 'manifests/' + name
        raw_dirs.append(path.parent)
    save(dest / 'index.portable.json', portable)
    subprocess.run([sys.executable, 'scripts/perf/summarize_architecture_candidates.py',
                    '--index', str(dest / 'index.portable.json'), '--output', str(dest)], check=True)
    result = json.loads((dest / 'results.json').read_text())
    groups[destination] = {'cases': result['case_count'],
        'statuses': dict(Counter(c['status'] for c in result['cases'])),
        'quality_flags': dict(Counter(f for g in result['groups'] for f in g['quality_flags']))}

# A derived view of existing observations, not additional experiments. Keep
# every initial Edge case; tables may select the explicitly rechecked groups.
combined_dir = evidence / 'regression-combined'
combined_dir.mkdir(exist_ok=True)
combined = {'status': 'completed', 'runs': [], 'derived_view_only': True,
            'initial_round_is_always_retained': True}
for name in ['tls-h2', 'fresh', 'regression-recheck']:
    index_path = evidence / name / 'index.portable.json'
    for entry in json.loads(index_path.read_text())['runs']:
        if entry['label'] not in {'baseline', 'candidate'}:
            continue
        entry = copy.deepcopy(entry)
        entry['manifest'] = os.path.relpath(index_path.parent / entry['manifest'], combined_dir)
        entry['source_group'] = name
        combined['runs'].append(entry)
save(combined_dir / 'index.portable.json', combined)
subprocess.run([sys.executable, 'scripts/perf/summarize_architecture_candidates.py',
                '--index', str(combined_dir / 'index.portable.json'),
                '--output', str(combined_dir)], check=True)

functional = {}
for name in ['functional-128k', 'functional-h3']:
    path = local / name / 'manifest.json'
    manifest = json.loads(path.read_text())
    assert manifest['status'] == 'completed' and not manifest.get('cleanup_errors')
    assert metadata['binaries']['candidate']['sha256'] in manifest['binaries'].values()
    dest = evidence / name
    dest.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, dest / 'manifest.json')
    functional[name] = {'cases': len(manifest['cases']),
        'statuses': dict(Counter(c['status'] for c in manifest['cases'])),
        'successful': sum(c.get('native', {}).get('successful', 0) for c in manifest['cases']),
        'failed': sum(c.get('native', {}).get('failed', 0) for c in manifest['cases'])}
    raw_dirs.append(path.parent)

tls_negotiation = {}
for target in ['edge', 'nginx', 'openresty']:
    source = local / ('tls-negotiation-' + target)
    manifest = json.loads((source / 'manifest.json').read_text())
    assert manifest['status'] == 'completed' and not manifest.get('cleanup_errors')
    dest = evidence / source.name
    dest.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source / 'manifest.json', dest / 'manifest.json')
    observations = []
    for path in sorted(source.glob('*.tls-negotiation.json')):
        observed = json.loads(path.read_text())
        assert observed['returncode'] == 0 and observed['result'].strip() == '200 1.1 0'
        shutil.copy2(path, dest / path.name)
        observations.append(observed)
    assert observations
    tls_negotiation[target] = {'diagnostic_only': True, 'observations': observations}
    raw_dirs.append(source)

profiles = {}
for label in ['before', 'after']:
    path = local / ('profiles-' + label) / 'manifest.json'
    manifest = json.loads(path.read_text())
    assert manifest['status'] == 'completed' and not manifest.get('cleanup_errors')
    expected = metadata['binaries']['baseline' if label == 'before' else 'candidate']['sha256']
    assert expected in manifest['binaries'].values()
    dest = evidence / ('profiles-' + label)
    dest.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, dest / 'manifest.json')
    for report in path.parent.glob('*.perf-flat.txt'):
        shutil.copy2(report, dest / report.name)
    summary = json.loads((local / ('profiles-' + label + '-summary.json')).read_text())
    profiles[label] = {'diagnostic_only': True, 'sample_counts': [
        p['sample_count'] for p in summary['profiles']],
        'lost_samples': sum(p['lost_samples'] for p in summary['profiles'])}
    raw_dirs.append(path.parent)

raw_hashes = []
for directory in raw_dirs:
    for path in sorted(directory.iterdir()):
        if path.is_file() and path.name.endswith(('.tsv.gz', '.stderr', '.command.json', '.log', '.headers', '.stacks.txt')):
            raw_hashes.append(sha(path) + '  ' + str(path.relative_to(root)))
(evidence / 'raw-files.sha256').write_text('\n'.join(raw_hashes) + '\n')
save(evidence / 'cpu-breakdown.json', records)
save(evidence / 'measurement-validation.json', {
    'all_candidate_source_and_binary_hashes_match': True,
    'comparison_case_count': len(records),
    'comparison_statuses': dict(Counter(r['status'] for r in records)),
    'candidate_statuses': dict(Counter(r['status'] for r in records if r['label'] == 'candidate')),
    'known_native_failed_transfers': sum(r['failed'] or 0 for r in records),
    'groups': groups, 'functional_only': functional, 'profiles': profiles,
    'tls_negotiation': tls_negotiation,
    'raw_file_hash_count': len(raw_hashes), 'all_failed_cases_retained': True,
    'fresh_connection_protocols_compared': ['h1', 'h1-tls', 'h2'],
    'long_stability_completed': False, 'h3_cache_fixed': False,
    'production_deployed': False, 'parent_cpu_quota_cores': 4})
(evidence / 'drivers').mkdir(exist_ok=True)
for name in ['run-comparisons.py', 'recheck-regressions.py', 'run-followups.py',
             'check-tls-negotiation.py', 'compare-uniform-tls.py', 'openssl-x25519.cnf',
             'render-tables.py', 'archive-evidence.py', 'record-build.py']:
    shutil.copy2(local / name, evidence / 'drivers' / name)
print(json.dumps({'comparison_cases': len(records), 'raw_hashes': len(raw_hashes)}))
