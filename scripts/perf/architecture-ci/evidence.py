#!/usr/bin/env python3
"""Check binary provenance and retain symbolized diagnostics without stack dumps."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/architecture-profile-private'
PUBLIC = ROOT / '.local/architecture-profile-ci'
BINARIES = ROOT / '.local/architecture-profile-binaries'
BINARY_SOURCE = 'a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a'
EXPECTED = {
    'target/release/Zelvik-Edge': '291c5cae4857d20c3f945d99e01a909740bfe0e54687e5cef4e131b817812c25',
    'target/release/examples/issue34-controller': '1a85adaec923f45c490b2bcc949adb35c562241e01024b3020aae1c99486b6b2',
    '.local/http-state-ci/protocol-state-client': '368312b058eb6a98373260a6b4977beb1655744418406c7c1c1bcedce92b3c1b',
}


def digest(path):
    sha = hashlib.sha256()
    with path.open('rb') as source:
        while block := source.read(1024 * 1024):
            sha.update(block)
    return sha.hexdigest()


def module(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def prepare():
    PRIVATE.mkdir(parents=True, exist_ok=True)
    for relative, expected in EXPECTED.items():
        assert digest(BINARIES / relative) == expected, relative
    metadata = {'harness_source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                'binary_source': BINARY_SOURCE, 'binary_run': 41665387055, 'binary_artifact': 13250758275,
                'expected_binaries': EXPECTED, 'capacity_numbers_excluded': True,
                'scope': 'CPU attribution of the exact final default HTTP-matrix binary; not a fresh capacity comparison',
                'source_hashes': {name: digest(ROOT / name) for name in [
                    'scripts/perf/profile_architecture.py', 'scripts/perf/protocol_state_matrix.py',
                    'scripts/perf/summarize_architecture.py', 'scripts/perf/http-state/verify.py',
                    'scripts/perf/architecture-ci/evidence.py', 'scripts/perf/architecture-ci/run.sh']}}
    (PRIVATE / 'provenance.json').write_text(json.dumps(metadata, indent=2) + '\n')


def verify():
    provenance = json.loads((PRIVATE / 'provenance.json').read_text())
    assert provenance['binary_source'] == BINARY_SOURCE
    for relative, expected in EXPECTED.items():
        assert digest(BINARIES / relative) == expected
    checked = module('checked_http', 'scripts/perf/http-state/verify.py')
    profiles = module('checked_profiles', 'scripts/perf/summarize_architecture.py')
    path = PRIVATE / 'profile/manifest.json'
    manifest = json.loads(path.read_text())
    rows = checked.validate(path, PRIVATE)
    assert len(rows) == 18 and all(row['status'] == 'passed' and row['failed'] == 0 for row in rows)
    assert {(row['protocol'], row['cache']) for row in rows} == {
        (protocol, state) for protocol in ['h1', 'h1-tls', 'h2'] for state in ['off', 'hit', 'miss']}
    samples = []
    for case in manifest['cases']:
        profile = profiles.profile(path.parent / (case['stem'] + '.stacks.txt'))
        profile.update(protocol=case['protocol'], cache=case['cache'], repeat=case['repeat'],
                       successful=case['native']['successful'], failed=case['native']['failed'])
        samples.append(profile)
    result = {'provenance': provenance, 'profiles': samples,
              'successful': sum(row['successful'] for row in rows), 'failed': sum(row['failed'] for row in rows),
              'max_host_steal_fraction': max(row['max_host_steal_fraction'] for row in rows),
              'capacity_numbers_excluded': True, 'inclusive_groups_overlap': True,
              'total_samples': sum(row['sample_count'] for row in samples),
              'lost_samples': sum(row['lost_samples'] for row in samples)}
    (PRIVATE / 'verified-profiles.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key not in ['provenance', 'profiles']}))


def collect():
    PUBLIC.mkdir(parents=True, exist_ok=True)
    retained = []
    # An explicit allow-list keeps perf.data (raw sampled stack bytes), TLS
    # keys, snapshots containing certificates, node stores and overrides out.
    exact = {'provenance.json', 'verified-profiles.json', 'curl-version.txt', 'perf-version.txt',
             'kernel.txt', 'packages.txt', 'manifest.json', 'payload'}
    suffixes = ('.perf-flat.txt', '.perf-callers.txt', '.stacks.txt', '.perf-record.log',
                '.perf-command.json', '.client.tsv.gz', '.client.tsv', '.stderr', '.command.json')
    if PRIVATE.exists():
        for path in PRIVATE.rglob('*'):
            if not path.is_file() or not (path.name in exact or path.name.endswith(suffixes)):
                continue
            relative = path.relative_to(PRIVATE)
            target = PUBLIC / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            retained.append({'path': str(relative), 'bytes': target.stat().st_size, 'sha256': digest(target)})
    (PUBLIC / 'retained-manifest.json').write_text(json.dumps({'files': retained,
        'excluded': ['perf.data and raw stack memory', 'TLS private material', 'runtime snapshots and stores']}, indent=2) + '\n')
    print(json.dumps({'retained_files': len(retained)}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['prepare', 'verify', 'collect'])
    globals()[parser.parse_args().mode]()
