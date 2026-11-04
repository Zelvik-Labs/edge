#!/usr/bin/env python3
"""Validate interoperability counts and dependency identities without ranking."""
import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[3]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(root, binaries):
    spec = importlib.util.spec_from_file_location('interop_verify', ROOT / 'scripts/perf/http-state/verify.py')
    http = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(http)
    suite = json.loads((root / 'suite.json').read_text())
    build = json.loads((root / 'client-build.json').read_text())
    mode = suite.get('client_mode', 'tls-backend')
    assert build.get('client_mode', 'tls-backend') == mode
    if mode == 'tls-backend':
        plans = {'openssl': {'binary': 'openssl', 'curves': None}, 'gnutls': {'binary': 'gnutls', 'curves': None}}
    elif mode == 'groups':
        plans = {'default': {'binary': 'openssl', 'curves': None}, 'x25519': {'binary': 'openssl', 'curves': 'X25519'}}
    else:
        assert mode == 'pacing'
        plans = {'unpaced': {'binary': 'openssl', 'curves': None, 'interval_us': 0},
                 'paced': {'binary': 'openssl', 'curves': None, 'interval_us': 100}}
        assert digest(root / 'source/relay.py') == suite['relay_source_sha256']
    if mode in ['groups', 'pacing']:
        assert digest(root / 'source/quic_client_diagnostics.h') == build['diagnostic_header_sha256']
        assert '-DEDGE_QUIC_DIAGNOSTICS' in build['clients']['openssl']['command']
    if mode == 'tls-backend':
        assert all('-DEDGE_QUIC_DIAGNOSTICS' not in row['command'] for row in build['clients'].values())
    assert suite['client_package_version'] == build['package_version']
    assert len(set(build['package_version'].values())) == 1
    assert build['source_sha256'] == suite['client_source_sha256']
    assert digest(root / 'source/protocol_state_client.c') == suite['client_source_sha256']
    for name, expected in suite['build']['binaries'].items():
        assert digest(binaries / name) == expected
    for name, expected in build['clients'].items():
        assert digest(binaries.parent / 'quinn-interop-private/clients' / name) == expected['sha256']
        for path, details in expected['libraries'].items():
            library = binaries.parent / 'quinn-interop-private/client-libraries' / Path(path).name
            assert library.stat().st_size == details['bytes'] and digest(library) == details['sha256']
    assert set(suite['clients']) == set(plans)
    for label, plan in plans.items():
        expected = build['clients'][plan['binary']]
        actual = suite['clients'][label]
        assert expected['sha256'] == actual['sha256'] and expected['version'] == actual['version']
        assert actual.get('binary', label) == plan['binary'] and actual.get('curves') == plan['curves']
        if mode == 'pacing':
            assert actual['interval_us'] == plan['interval_us']
    if mode in ['groups', 'pacing']:
        assert len({row['sha256'] for row in suite['clients'].values()}) == 1
    labels = list(plans)
    assert [(r['round'], r['client']) for r in suite['runs']] == [
        (i, label) for i, order in enumerate([labels, list(reversed(labels))]) for label in order]
    records, infrastructure, partial, groups, relays = [], [], [], [], []

    def verify_relay(path, client, cpu):
        relay = json.loads(path.read_text())
        assert relay['source_sha256'] == suite['relay_source_sha256']
        assert relay['error'] is None
        interval = plans[client]['interval_us']
        assert relay['interval_us'] == interval
        assert relay['affinity'] == [relay['cpu']] == [cpu]
        assert relay['wall_ns'] > 0 and relay['thread_cpu_ns'] > 0
        assert 0 < relay['flows'] <= 20_000
        assert relay['received_client'] == relay['forwarded_client'] > 0
        assert relay['received_server'] >= relay['forwarded_server'] > 0
        assert relay['received_server'] - relay['forwarded_server'] == relay['queued_at_shutdown']
        assert 0 <= relay['queued_at_shutdown'] <= relay['peak_queued'] <= 32_768
        assert 0 <= relay['bytes_at_shutdown'] <= relay['peak_bytes'] <= 64 * 1024 * 1024
        if relay['forwarded_server'] > 1:
            assert relay['minimum_server_send_interval_ns'] >= interval * 1000
        relays.append({'client': client, 'file': str(path.relative_to(root)), **relay})
        return relay
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
        assert args.get('client_curves') == plans[run['client']]['curves']
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
        if mode == 'pacing':
            relay = verify_relay(path.parent / (case['stem'] + '.relay.json'), run['client'],
                                 args['cpu_assignment']['origin'][0])
            expected_url = f"https://localhost:{case['ports']['19443/udp']}/hit/payload"
            assert relay['target_url'] == expected_url
            commands = json.loads((path.parent / (case['stem'] + '.command.json')).read_text())
            assert len(commands) == 2
            for command in commands:
                assert command[command.index('--url') + 1] == relay['client_url']
        if case.get('native') is not None:
            assert row.get('successful', 0) + row.get('failed', 0) == 16384
            if mode in ['groups', 'pacing']:
                observations = 0
                for shard in range(2):
                    text = (path.parent / (case['stem'] + f'.{shard}.stderr')).read_text()
                    slots = re.findall(r'quic_group_summary slot=(\d+) observations=(\d+) last_group=(\d+) changed=(\d+)', text)
                    assert len(slots) == 16 and {int(s[0]) for s in slots} == set(range(16))
                    slots = [{'slot': int(s), 'observations': int(n), 'last_group': int(g), 'changed': int(c)} for s, n, g, c in slots]
                    observations += sum(s['observations'] for s in slots)
                    assert all(s['last_group'] > 0 for s in slots if s['observations'])
                    if run['client'] == 'x25519':
                        assert all(s['last_group'] == 29 and s['changed'] == 0 for s in slots if s['observations'])
                    groups.append({'client': run['client'], 'round': run['round'], 'shard': shard, 'slots': slots,
                        'scope': 'wire-observed selected key-share group; not authentication on its own'})
                assert observations >= row['successful'] and observations <= 16384
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
            if plans[control['client']]['curves']:
                position = control['command'].index('--curves')
                assert control['command'][position + 1] == plans[control['client']]['curves']
            if mode == 'pacing':
                paths = list((root / control['case']).glob('*.wrong-ca-relay.json'))
                assert len(paths) == 1
                relay = json.loads(paths[0].read_text())
                verify_relay(paths[0], control['client'], relay['cpu'])
                assert control['command'][control['command'].index('--url') + 1] == relay['client_url']
    by_client = defaultdict(list)
    for row in records:
        by_client[row['client']].append(row)
    summary = {'source': suite['source'], 'runtime_source': suite['build']['runtime_source'],
        'scope': suite['scope'], 'cases': len(records), 'infrastructure_failures': infrastructure,
        'incomplete_processes': partial, 'wrong_ca_rejections': len(controls), 'group_observations': groups,
        'client_mode': mode, 'clients': {}}
    if mode == 'pacing':
        summary['relay_observations'] = relays
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
