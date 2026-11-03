#!/usr/bin/env python3
"""Verify retained Linux ingress, wire and default native regression evidence."""
import hashlib
import itertools
import json
from pathlib import Path
import re
import zipfile

ROOT = Path(__file__).resolve().parent

for directory in sorted(ROOT.iterdir()):
    if not directory.is_dir():
        continue
    manifest = json.loads((directory / 'manifest.json').read_text())
    artifact = json.loads((directory / 'artifact.json').read_text())
    raw = directory / 'raw.zip'
    digest = hashlib.file_digest(raw.open('rb'), 'sha256').hexdigest()
    assert raw.stat().st_size == manifest['archive_bytes'] == artifact['size_in_bytes']
    assert digest == manifest['archive_sha256'] == artifact['digest'].removeprefix('sha256:')
    with zipfile.ZipFile(raw) as archive:
        assert not any(name.endswith('/key.pem') for name in archive.namelist())
        for entry in manifest['files']:
            body = archive.read(entry['path'])
            assert len(body) == entry['bytes']
            assert hashlib.sha256(body).hexdigest() == entry['sha256']
        if directory.name == 'root-final-native':
            for parts in itertools.product(['copied', 'owned'], ['eager', 'cooperative'], ['full', 'bridge'], ['1', '2'], ['worker', 'readable']):
                label = '-'.join(parts)
                row = json.loads(archive.read(label + '.json'))
                assert row['passed'] and row['exit_code'] == 0 and row['network_cleanup_ok']
                assert row['remaining_namespaces'] == ''
                assert len(row['remaining_links']) == 1 and row['remaining_links'][0]['ifname'] == 'lo'
                assert b'xdp-smoke: smoke completed' in archive.read(label + '.log')
            print('PASS: 32 native cases and exact network cleanup')
        elif directory.name == 'quiche-bounded-ingress-pass':
            log = archive.read('tests.log').decode()
            assert 'test result: ok. 1 passed; 0 failed' in log
            assert 'test result: ok. 8 passed; 0 failed' in log
            for address in ['bounded-ingress 127.0.0.1:', 'bounded-ingress [::1]:']:
                assert address in log
            assert log.count('whole valid datagrams preserved, 1501/65000 rejected, GSO split into 2x1350') == 2
            assert 'loss fixture dropped server/client datagrams: [48, 46]' in log
            for repeat in range(1, 11):
                assert b'test result: ok. 1 passed; 0 failed' in archive.read(f'repeat-{repeat}.log')
            for name in ['clippy.log', 'tests.log']:
                text = re.sub(r'\x1b\[[0-9;]*m', '', archive.read(name).decode())
                assert not any(line.startswith(('warning:', 'warning[')) for line in text.splitlines())
            print('PASS: IPv4/IPv6 packet bound, 8 wire cases and 10 multiplex repeats')
        elif directory.name == 'fin-control-lock-failure':
            log = archive.read('baseline-lock.log').decode()
            assert 'no matching package named `boring` found' in log
            assert 'status.txt' not in archive.namelist()
            print('Retained setup failure: offline index, no FIN test outcome')
        elif directory.name == 'fin-control-bounded-pass':
            assert archive.read('status.txt').decode() == 'old_status=101\nfixed_status=0\n'
            old = archive.read('old.log').decode()
            assert 'queued FIN was discarded before its first packet was sent' in old
            assert 'left: Err(Done)' in old and 'right: Ok((0, true))' in old
            assert b'test result: ok. 1 passed; 0 failed' in archive.read('fixed.log')
            def identities(name):
                text = archive.read(name).decode()
                return {tuple(re.findall(r'^(?:name|version|source) = (.*)$', section, re.M)) for section in text.split('[[package]]')[1:]}
            assert identities('baseline-before.lock') == identities('baseline-after.lock')
            print('PASS: final adapter old/fixed FIN control and unchanged baseline package identities')
        elif directory.name in ['quiche-node-bounded-pass', 'quiche-node-diagnostic-pass']:
            for name in ['check.log', 'clippy.log', 'tests.log']:
                text = re.sub(r'\x1b\[[0-9;]*m', '', archive.read(name).decode())
                assert not any(line.startswith(('warning:', 'warning[')) for line in text.splitlines())
            log = archive.read('tests.log').decode()
            for count in [1343, 15, 1]:
                assert f'test result: ok. {count} passed; 0 failed;' in log
            assert '1343 passed; 0 failed; 4 ignored' in log
            print('PASS: full feature check, Clippy and 1343+16 tests; 4 explicit ignored probes')
        elif directory.name in ['quiche-admission-build-failure', 'quiche-admission-node-build-failure']:
            name = 'build.log' if directory.name == 'quiche-admission-build-failure' else 'check.log'
            assert b'cannot find macro `warn` in this scope' in archive.read(name)
            print('Retained diagnostic compile failure: missing qualified tracing macro')

print('PASS: all archive and per-file hashes verified')
