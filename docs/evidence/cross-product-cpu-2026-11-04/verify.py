#!/usr/bin/env python3
"""Recompute successful profiles and preserve the first diagnostic failure."""
import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from verify_archive import verify_archive


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def inspect(root, frozen, failed):
    http = module('cross_http', frozen / 'http_verify.py')
    profiles = module('cross_profiles', frozen / 'profile_summary.py')
    suite = json.loads((root / 'suite.json').read_text())
    provenance = json.loads((root / 'provenance.json').read_text())
    expected_runs = [(i, target) for i, order in enumerate(suite['orders']) for target in order]
    actual_runs = [(r['round'], r['target']) for r in suite['runs']]
    assert actual_runs == (expected_runs[:2] if failed else expected_runs)
    result = {'source': suite['source'], 'binary_source': provenance['binary_source'],
              'scope': 'user CPU only; instrumented timing excluded from capacity ranking',
              'complete_cells': 0, 'diagnostic_failures': 0, 'unrun_cells': 16 if failed else 0,
              'successful': 0, 'samples': 0, 'lost_samples': 0, 'max_host_steal_fraction': 0,
              'profiles': []}
    combined = defaultdict(lambda: {'samples': 0, 'unknown_leaf': 0, 'groups': Counter(), 'leaves': Counter()})
    for run in suite['runs']:
        path = root / run['manifest']
        data = json.loads(path.read_text())
        assert data['source_revision'] == suite['source']
        assert data['client_sha256'] == provenance['expected_binaries']['.local/http-state-ci/protocol-state-client']
        assert not data.get('cleanup_errors') and len(data['cases']) == 4
        if failed and run['target'] == 'nginx':
            assert run['exit'] == 1
            for case in data['cases']:
                stem = path.parent / case['stem']
                assert case['status'] == 'failed' and 'exit status 141' in case['error']
                assert Path(str(stem) + '.perf-flat.txt').is_file()
                assert not Path(str(stem) + '.stacks.txt').exists()
                assert not list(path.parent.glob(case['stem'] + '*.client.tsv*'))
                result['diagnostic_failures'] += 1
            result['missing_failed_transfer_records'] = True
            continue
        assert run['exit'] == 0
        transfers = http.validate(path, root)
        assert len(transfers) == 4 and all(r['status'] == 'passed' and r['failed'] == 0 for r in transfers)
        result['successful'] += sum(r['successful'] for r in transfers)
        result['max_host_steal_fraction'] = max(result['max_host_steal_fraction'], *(r['max_host_steal_fraction'] for r in transfers))
        for case in data['cases']:
            stem = path.parent / case['stem']
            target = json.loads(Path(str(stem) + '.profile-target.json').read_text())
            expected = provenance['expected_binaries']['target/release/Zelvik-Edge'] if run['target'] == 'edge' else hashlib.sha256((path.parent / 'profile-executable').read_bytes()).hexdigest()
            assert target['target'] == run['target'] and target['executable_sha256'] == expected
            assert target['container_pids'] and all(pid > 0 for pid in target['container_pids'])
            if run['target'] != 'edge':
                assert suite['images'][run['target']]['executable_sha256'] == expected
            sample_path = Path(str(stem) + '.stacks.txt')
            row = profiles.profile(sample_path)
            row.update(path=str(sample_path.relative_to(root)), round=run['round'], target=run['target'], protocol=case['protocol'], cache=case['cache'])
            blocks = [b for b in sample_path.read_text().split('\n\n') if 'cpu-clock:u:' in b]
            leaves = Counter(re.sub(r'\+0x[0-9a-f]+.*', '', b.splitlines()[1].strip().split(' ', 1)[1]) for b in blocks)
            row['unknown_leaf_samples'] = sum(n for symbol, n in leaves.items() if symbol.startswith('[unknown]'))
            result['profiles'].append(row)
            result['samples'] += row['sample_count']
            result['lost_samples'] += row['lost_samples']
            result['complete_cells'] += 1
            aggregate = combined[run['target'], case['protocol'], case['cache']]
            aggregate['samples'] += row['sample_count']
            aggregate['unknown_leaf'] += row['unknown_leaf_samples']
            aggregate['leaves'].update(leaves)
            aggregate['groups'].update({k: v['samples'] for k, v in row['groups'].items()})
    result['aggregate'] = []
    for (target, protocol, cache), data in sorted(combined.items()):
        result['aggregate'].append(dict(target=target, protocol=protocol, cache=cache,
            samples=data['samples'], unknown_leaf_percent=data['unknown_leaf'] * 100 / data['samples'],
            inclusive_groups={k: n * 100 / data['samples'] for k, n in data['groups'].items()},
            leaves=[dict(symbol=s, samples=n, percent=n * 100 / data['samples']) for s, n in data['leaves'].most_common(30)]))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('download', type=Path)
    parser.add_argument('binaries', type=Path, help='original immutable header-node binary download')
    parser.add_argument('client', type=Path, help='original actual C client executable')
    parser.add_argument('--stage', choices=['pass', 'first-failure', 'retention-repeat', 'compatibility-repeat'], default='pass')
    parser.add_argument('--write-summary', type=Path)
    args = parser.parse_args()
    frozen = HERE / args.stage
    expected = json.loads((frozen / 'artifact.json').read_text())
    for name in ['artifact.json', 'manifest.json']:
        assert json.loads((args.download / name).read_text()) == json.loads((frozen / name).read_text())
    root = verify_archive(args.download, expected['workflow_run']['head_sha'])
    binaries = verify_archive(args.binaries, '4171ea700386d8ed7c755255c7eeccb7e804d644')
    provenance = json.loads((root / 'provenance.json').read_text())
    for relative, expected_sha in provenance['expected_binaries'].items():
        path = args.client if relative.endswith('protocol-state-client') else binaries / 'cache-range-binaries/after' / Path(relative).name
        assert hashlib.sha256(path.read_bytes()).hexdigest() == expected_sha
    result = json.loads(json.dumps(inspect(root, frozen, args.stage == 'first-failure')))
    if args.write_summary:
        args.write_summary.write_text(json.dumps(result, indent=2) + '\n')
    else:
        assert result == json.loads((frozen / 'verification.json').read_text())
    print(json.dumps({k: v for k, v in result.items() if k not in ['profiles', 'aggregate']}))


if __name__ == '__main__':
    main()
