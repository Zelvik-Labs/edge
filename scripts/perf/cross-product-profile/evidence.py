#!/usr/bin/env python3
"""Keep cross-product CPU stacks distinct from capacity comparisons."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/cross-product-profile-private'
PUBLIC = ROOT / '.local/cross-product-profile-ci'
IMAGES = {'edge': 'edge-architecture-profiler:20261104',
          'nginx': 'edge-nginx-profiler:20261104', 'openresty': 'edge-openresty-profiler:20261104'}
ORDERS = [['edge', 'nginx', 'openresty'], ['openresty', 'nginx', 'edge']]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def prepare():
    original = module('original_profile_evidence', 'scripts/perf/architecture-ci/evidence.py')
    original.PRIVATE = PRIVATE
    original.prepare()


def run():
    prepare()
    bins = ROOT / '.local/architecture-profile-binaries'
    suite = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
             'scope': 'user CPU stacks only; all instrumented timing and normalized CPU excluded from capacity rankings',
             'orders': ORDERS, 'images': {}, 'runs': []}
    for name, image in {**IMAGES, 'origin': 'nginx:1.28.0'}.items():
        details = json.loads(subprocess.check_output(['docker', 'image', 'inspect', image], text=True))[0]
        suite['images'][name] = {'ref': image, 'id': details['Id'], 'repo_digests': details['RepoDigests']}
    for name, image, executable in [
        ('nginx', 'nginx:1.28.0', '/usr/sbin/nginx'),
        ('openresty', 'openresty/openresty:1.27.1.2-0-bookworm-fat', '/usr/local/openresty/nginx/sbin/nginx'),
    ]:
        details = json.loads(subprocess.check_output(['docker', 'image', 'inspect', image], text=True))[0]
        expected = subprocess.check_output(['docker', 'run', '--rm', '--entrypoint', 'sha256sum', image, executable], text=True).split()[0]
        actual = subprocess.check_output(['docker', 'run', '--rm', '--entrypoint', 'sha256sum', IMAGES[name], executable], text=True).split()[0]
        assert expected == actual
        suite['images'][name].update(base_ref=image, base_id=details['Id'], base_repo_digests=details['RepoDigests'], executable_sha256=actual)
    (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
    for round_number, order in enumerate(ORDERS):
        for target in order:
            output = PRIVATE / f'round-{round_number}-{target}'
            command = ['python3', 'scripts/perf/profile_architecture.py', '--target', target,
                '--output', str(output), '--tls', str(PRIVATE / 'tls'),
                '--client', str(bins / '.local/http-state-ci/protocol-state-client'),
                '--edge-binary', str(bins / 'target/release/Zelvik-Edge'),
                '--controller', str(bins / 'target/release/examples/issue34-controller'),
                '--profiler-image', IMAGES[target], '--protocols', 'h1', 'h2',
                '--states', 'hit', 'off', '--repeats', '1', '--seconds', '20', '--frequency', '199']
            with (PRIVATE / f'{round_number}-{target}.driver.log').open('w') as log:
                result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
            suite['runs'].append({'round': round_number, 'target': target,
                'manifest': str(output.relative_to(PRIVATE) / 'manifest.json'), 'exit': result.returncode})
            (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
            if result.returncode:
                raise RuntimeError(f'profile failed: round {round_number} target {target}; partial evidence retained')
    verify()


def verify():
    suite = json.loads((PRIVATE / 'suite.json').read_text())
    assert [(r['round'], r['target']) for r in suite['runs']] == [
        (i, target) for i, order in enumerate(ORDERS) for target in order]
    http = module('cross_http', 'scripts/perf/http-state/verify.py')
    profiles = module('cross_profiles', 'scripts/perf/summarize_architecture.py')
    provenance = json.loads((PRIVATE / 'provenance.json').read_text())
    rows = []
    successful = 0
    steals = []
    for run_data in suite['runs']:
        assert run_data['exit'] == 0
        path = PRIVATE / run_data['manifest']
        manifest = json.loads(path.read_text())
        assert manifest['target'] == run_data['target'] and manifest['source_revision'] == suite['source']
        assert manifest['client_sha256'] == provenance['expected_binaries']['.local/http-state-ci/protocol-state-client']
        transfers = http.validate(path, PRIVATE)
        assert len(transfers) == 4 and all(r['status'] == 'passed' and r['failed'] == 0 for r in transfers)
        assert {(r['protocol'], r['cache']) for r in transfers} == {('h1', 'hit'), ('h1', 'off'), ('h2', 'hit'), ('h2', 'off')}
        successful += sum(r['successful'] for r in transfers)
        steals.extend(r['max_host_steal_fraction'] for r in transfers)
        for case in manifest['cases']:
            stem = path.parent / case['stem']
            target = json.loads(Path(str(stem) + '.profile-target.json').read_text())
            if run_data['target'] == 'edge':
                expected = provenance['expected_binaries']['target/release/Zelvik-Edge']
            else:
                expected = digest(path.parent / 'profile-executable')
                assert expected == suite['images'][run_data['target']]['executable_sha256']
            assert target['target'] == run_data['target'] and target['executable_sha256'] == expected
            assert target['container_pids'] and all(pid > 0 for pid in target['container_pids'])
            data = profiles.profile(Path(str(stem) + '.stacks.txt'))
            data.update(round=run_data['round'], target=run_data['target'], protocol=case['protocol'], cache=case['cache'])
            rows.append(data)
    summary = {'source': suite['source'], 'binary_source': provenance['binary_source'],
               'scope': suite['scope'], 'complete_cells': len(rows), 'successful': successful,
               'failed': 0, 'samples': sum(r['sample_count'] for r in rows),
               'lost_samples': sum(r['lost_samples'] for r in rows), 'max_host_steal_fraction': max(steals),
               'inclusive_groups_overlap': True, 'profiles': rows}
    (PRIVATE / 'verified-profiles.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({key: value for key, value in summary.items() if key != 'profiles'}))


def collect():
    PUBLIC.mkdir(parents=True, exist_ok=True)
    exact = {'suite.json', 'provenance.json', 'verified-profiles.json', 'manifest.json', 'payload', 'profile-executable'}
    suffixes = ('.perf-flat.txt', '.perf-callers.txt', '.stacks.txt', '.perf-record.log', '.perf-command.json',
                '.client.tsv.gz', '.client.tsv', '.stderr', '.command.json', '.profile-target.json', '.driver.log',
                '.perf-flat-error.txt', '.perf-callers-error.txt')
    retained = []
    if PRIVATE.exists():
        for path in PRIVATE.rglob('*'):
            if path.is_file() and (path.name in exact or path.name.endswith(suffixes)):
                relative = path.relative_to(PRIVATE)
                output = PUBLIC / relative
                output.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, output)
                retained.append({'path': str(relative), 'bytes': output.stat().st_size, 'sha256': digest(output)})
    (PUBLIC / 'retained-manifest.json').write_text(json.dumps({'files': retained,
        'excluded': ['raw perf.data and stack memory', 'TLS private keys', 'runtime snapshots and stores']}, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['prepare', 'run', 'verify', 'collect'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        PRIVATE = args.directory.resolve()
    globals()[args.mode]()
