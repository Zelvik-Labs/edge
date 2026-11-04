#!/usr/bin/env python3
"""Validate interoperability counts and dependency identities without ranking."""
import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(root, binaries):
    spec = importlib.util.spec_from_file_location('interop_verify', ROOT / 'scripts/perf/http-state/verify.py')
    http = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(http)
    suite = json.loads((root / 'suite.json').read_text())
    build = json.loads((root / 'client-build.json').read_text())
    assert suite['client_package_version'] == build['package_version']
    assert len(set(build['package_version'].values())) == 1
    assert build['source_sha256'] == suite['client_source_sha256']
    assert digest(root / 'source/protocol_state_client.c') == suite['client_source_sha256']
    for name, expected in suite['build']['binaries'].items():
        assert digest(binaries / name) == expected
    for name, expected in build['clients'].items():
        assert expected['sha256'] == suite['clients'][name]['sha256']
        assert expected['version'] == suite['clients'][name]['version']
        assert digest(binaries.parent / 'quinn-interop-private/clients' / name) == expected['sha256']
        for path, details in expected['libraries'].items():
            library = binaries.parent / 'quinn-interop-private/client-libraries' / Path(path).name
            assert library.stat().st_size == details['bytes'] and digest(library) == details['sha256']
    assert [(r['round'], r['client']) for r in suite['runs']] == [(0, 'openssl'), (0, 'gnutls'), (1, 'gnutls'), (1, 'openssl')]
    records, infrastructure, partial = [], [], []
    for run in suite['runs']:
        if run['infrastructure_error']:
            infrastructure.append(run)
            continue
        path = root / run['manifest']
        manifest = json.loads(path.read_text())
        assert manifest['source_revision'] == suite['source']
        assert manifest['client_source_sha256'] == suite['client_source_sha256']
        assert manifest['client_sha256'] == suite['clients'][run['client']]['sha256']
        assert run['native_listener_startup'] and all('gso=Auto' in x for x in run['native_listener_startup'])
        args = manifest['args']
        assert args.get('fixture_ca_only', False) == suite.get('client_fixture_ca_only', False)
        assert args['edge_backend'] == 'quinn-native' and args['transport_profile'] == 'matched'
        assert args['address_validation'] == 'off' and args['quinn_native_gso'] == 'auto'
        assert args['fresh_requests'] == suite['request_limit'] == 16384
        assert manifest['origin_readiness'] and not manifest['origin_reference']['failed']
        rows = http.validate(path, root)
        assert len(rows) == 1
        row = rows[0]
        assert row['payload_bytes'] == 32768 and row['protocol'] == 'h3' and row['cache'] == 'hit' and row['connection'] == 'fresh'
        row.update(round=run['round'], client=run['client'])
        case = manifest['cases'][0]
        if case.get('native') is not None:
            assert row.get('successful', 0) + row.get('failed', 0) == 16384
        else:
            assert row['status'] == 'failed' and row['error']
            for shard in range(2):
                path_raw = path.parent / (case['stem'] + f'.{shard}.client.tsv')
                lines = path_raw.read_text().splitlines(keepends=True) if path_raw.exists() else []
                trailing = 0
                if lines and not lines[-1].endswith('\n'):
                    trailing = len(lines.pop().encode())
                valid, invalid = 0, Counter()
                for line in lines:
                    fields = line.strip().split('\t')
                    assert len(fields) == 8
                    code, version, connects, received, seconds, first, error, connection = fields
                    code, connects, received, error = map(int, [code, connects, received, error])
                    float(seconds), float(first), int(connection)
                    if code == 200 and version == '3' and connects == 1 and received == 32768 and error == 0:
                        valid += 1
                    else:
                        invalid[f'curl_{error}/http_{code}/v_{version}/bytes_{received}/new_{connects}'] += 1
                partial.append({'client': run['client'], 'round': run['round'], 'shard': shard,
                    'retained_records': len(lines), 'valid_records': valid, 'invalid_records': sum(invalid.values()),
                    'error_distribution': dict(invalid), 'trailing_partial_bytes': trailing,
                    'scope': 'killed incomplete process; not a complete cell or a capacity sample'})
        records.append(row)
    controls = []
    if suite.get('client_fixture_ca_only'):
        controls = json.loads((root / 'tls-controls.json').read_text())
        assert len(controls) == 4
        for control in controls:
            fields = control['stdout'].strip().split('\t')
            assert control['exit'] == 0 and len(fields) == 8 and fields[0] == '0' and fields[6] == '60'
            assert '--fixture-ca-only' in control['command'] and 'wrong-ca.pem' in ' '.join(control['command'])
    by_client = defaultdict(list)
    for row in records:
        by_client[row['client']].append(row)
    summary = {'source': suite['source'], 'runtime_source': suite['build']['runtime_source'],
        'scope': suite['scope'], 'cases': len(records), 'infrastructure_failures': infrastructure,
        'incomplete_processes': partial, 'wrong_ca_rejections': len(controls), 'clients': {}}
    for client, rows in by_client.items():
        summary['clients'][client] = {'statuses': dict(Counter(r['status'] for r in rows)),
            'successful': sum(r.get('successful', 0) for r in rows), 'failed': sum(r.get('failed', 0) for r in rows),
            'complete_cases': sum(r.get('successful', 0) + r.get('failed', 0) == 16384 for r in rows),
            'version': suite['clients'][client]['version']}
    (root / 'verified-records.json').write_text(json.dumps(records, indent=2) + '\n')
    (root / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('binaries', type=Path)
    args = parser.parse_args()
    verify(args.root, args.binaries)
