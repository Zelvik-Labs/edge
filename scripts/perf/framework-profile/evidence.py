#!/usr/bin/env python3
"""Profile immutable Edge and its measured bare Pingora control independently."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/framework-profile-private'
PUBLIC = ROOT / '.local/framework-profile-ci'
BARE_SHA = '1a5439941c53dc32044b665013add230ac8a2062ab34e72f92dfde7799d093c2'
ORDERS = [['edge', 'pingora'], ['pingora', 'edge']]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def prepare():
    original = module('framework_original', 'scripts/perf/architecture-ci/evidence.py')
    original.PRIVATE = PRIVATE
    original.prepare()
    source = ROOT / '.local/framework-profile-input'
    build = json.loads((source / 'build-metadata.json').read_text())
    assert build['source'] == '3b25c01f2b0d40ad90beb20a41e7cff917e54510'
    assert build['edge_runtime'] == 'ba474745fae3c08332d9e0862d9d5650550664ff'
    assert sha(source / 'bare-pingora-proxy') == build['bare_binary']['sha256'] == BARE_SHA
    shutil.copy2(source / 'build-metadata.json', PRIVATE / 'bare-build-metadata.json')
    bins = ROOT / '.local/framework-profile-binaries'
    bins.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source / 'bare-pingora-proxy', bins / 'bare-pingora-proxy')
    (bins / 'bare-pingora-proxy').chmod((bins / 'bare-pingora-proxy').stat().st_mode | 0o111)


def run():
    prepare()
    bins = ROOT / '.local/architecture-profile-binaries'
    image = 'edge-architecture-profiler:20261104'
    suite = {'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
             'orders': ORDERS, 'bare_sha256': BARE_SHA, 'runs': [],
             'scope': 'OFF-only user CPU paths of fixed ba474 Edge vs fixed bare Pingora; instrumented timing excluded from capacity ranking',
             'image': json.loads(subprocess.check_output(['docker', 'image', 'inspect', image], text=True))[0]['Id']}
    (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
    for number, order in enumerate(ORDERS):
        for target in order:
            output = PRIVATE / f'round-{number}-{target}'
            with (PRIVATE / f'{number}-{target}.driver.log').open('w') as log:
                result = subprocess.run(['python3', 'scripts/perf/profile_architecture.py', '--target', target,
                    '--output', str(output), '--tls', str(PRIVATE / 'tls'),
                    '--client', str(bins / '.local/http-state-ci/protocol-state-client'),
                    '--edge-binary', str(bins / 'target/release/Zelvik-Edge'),
                    '--controller', str(bins / 'target/release/examples/issue34-controller'),
                    '--bare-pingora-binary', str(ROOT / '.local/framework-profile-binaries/bare-pingora-proxy'),
                    '--profiler-image', image, '--protocols', 'h1', 'h1-tls', 'h2',
                    '--states', 'off', '--repeats', '1', '--seconds', '20', '--frequency', '199'],
                    stdout=log, stderr=subprocess.STDOUT)
            suite['runs'].append(dict(round=number, target=target, manifest=str(output.relative_to(PRIVATE) / 'manifest.json'), exit=result.returncode))
            (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
            if result.returncode:
                raise RuntimeError(f'framework profile failed: round {number} target {target}; retain all partial records')
    verify()


def verify():
    http = module('framework_http', 'scripts/perf/http-state/verify.py')
    profiles = module('framework_profiles', 'scripts/perf/summarize_architecture.py')
    suite = json.loads((PRIVATE / 'suite.json').read_text())
    provenance = json.loads((PRIVATE / 'provenance.json').read_text())
    assert [(r['round'], r['target']) for r in suite['runs']] == [(i, t) for i, order in enumerate(ORDERS) for t in order]
    rows, transfers = [], []
    for run_data in suite['runs']:
        assert run_data['exit'] == 0
        path = PRIVATE / run_data['manifest']
        manifest = json.loads(path.read_text())
        assert manifest['source_revision'] == suite['source']
        assert manifest['client_sha256'] == provenance['expected_binaries']['.local/http-state-ci/protocol-state-client']
        expected = provenance['expected_binaries']['target/release/Zelvik-Edge'] if run_data['target'] == 'edge' else BARE_SHA
        if run_data['target'] == 'edge':
            assert sorted(manifest['binaries'].values()) == sorted(provenance['expected_binaries'][p] for p in ['target/release/Zelvik-Edge', 'target/release/examples/issue34-controller'])
        else:
            assert list(manifest['binaries'].values()) == [BARE_SHA]
            assert sha(path.parent / 'profile-executable') == BARE_SHA
        checked = http.validate(path, PRIVATE)
        assert len(checked) == 3 and all(r['status'] == 'passed' and r['failed'] == 0 for r in checked)
        assert {(r['protocol'], r['cache']) for r in checked} == {(p, 'off') for p in ['h1', 'h1-tls', 'h2']}
        transfers.extend(checked)
        for case in manifest['cases']:
            stem = path.parent / case['stem']
            identity = json.loads(Path(str(stem) + '.profile-target.json').read_text())
            assert identity['target'] == run_data['target'] and identity['executable_sha256'] == expected
            assert identity['container_pids'] and all(pid > 0 for pid in identity['container_pids'])
            row = profiles.profile(Path(str(stem) + '.stacks.txt'))
            row.update(path=str(Path(str(stem) + '.stacks.txt').relative_to(PRIVATE)),
                       round=run_data['round'], target=run_data['target'], protocol=case['protocol'], cache=case['cache'])
            rows.append(row)
    summary = {'source': suite['source'], 'scope': suite['scope'], 'edge_source': provenance['binary_source'],
               'bare_sha256': BARE_SHA, 'cells': len(rows), 'successful': sum(r['successful'] for r in transfers),
               'failed': sum(r['failed'] for r in transfers), 'samples': sum(r['sample_count'] for r in rows),
               'lost_samples': sum(r['lost_samples'] for r in rows),
               'max_host_steal_fraction': max(r['max_host_steal_fraction'] for r in transfers), 'profiles': rows}
    (PRIVATE / 'verified-profiles.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k != 'profiles'}))


def collect():
    original = module('framework_collector', 'scripts/perf/cross-product-profile/evidence.py')
    original.PRIVATE, original.PUBLIC = PRIVATE, PUBLIC
    original.collect()
    path = PUBLIC / 'retained-manifest.json'
    manifest = json.loads(path.read_text())
    for source in PRIVATE.rglob('*'):
        if source.is_file() and (source.name == 'bare-build-metadata.json' or source.suffix in ['.body', '.headers']):
            relative = source.relative_to(PRIVATE)
            output = PUBLIC / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, output)
            manifest['files'].append(dict(path=str(relative), bytes=output.stat().st_size, sha256=sha(output)))
    path.write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['run', 'verify', 'collect'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        PRIVATE = args.directory.resolve()
    globals()[args.mode]()
