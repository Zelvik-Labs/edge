#!/usr/bin/env python3
"""Verify before/after qlog proof without exposing test private keys."""
import hashlib
import json
from pathlib import Path
import zipfile

root = Path(__file__).resolve().parent
for directory in sorted(root.iterdir()):
    if not directory.is_dir():
        continue
    manifest = json.loads((directory / 'manifest.json').read_text())
    raw = directory / 'raw.zip'
    assert raw.stat().st_size == manifest['archive_bytes']
    assert hashlib.file_digest(raw.open('rb'), 'sha256').hexdigest() == manifest['archive_sha256']
    rows = []
    with zipfile.ZipFile(raw) as archive:
        assert not any(name.endswith('/key.pem') for name in archive.namelist())
        for entry in manifest['files']:
            body = archive.read(entry['path'])
            assert len(body) == entry['bytes']
            assert hashlib.sha256(body).hexdigest() == entry['sha256']
            if not entry['path'].endswith('.sqlog'):
                continue
            streams = {}
            for record in body.decode().split('\x1e'):
                if not record.strip():
                    continue
                event = json.loads(record)
                if event.get('name') != 'quic:packet_sent':
                    continue
                for frame in event['data'].get('frames', []):
                    if frame.get('frame_type') != 'stream' or int(frame['stream_id']) % 4:
                        continue
                    stream = streams.setdefault(int(frame['stream_id']), {'end': 0, 'fin': False})
                    stream['end'] = max(stream['end'], int(frame.get('offset', 0)) + int(frame.get('length', 0)))
                    stream['fin'] |= bool(frame.get('fin'))
            if len(streams) == 16:
                rows.append({'fixture': Path(entry['path']).parent.parent.name,
                             'missing': {str(key): value for key, value in streams.items() if not value['fin']}})
        if directory.name == 'quiche-loss-late-fin-pass':
            tests = archive.read('tests.log').decode()
            assert 'test result: ok. 7 passed; 0 failed' in tests
            assert 'loss fixture dropped server/client datagrams: [49, 46]' in tests
            assert 'test fin_sent_after_all_body_bytes_are_acknowledged_still_reaches_peer ... ok' in tests
            for attempt in range(1, 11):
                assert 'test result: ok. 1 passed; 0 failed' in archive.read(f'repeat-{attempt}.log').decode()
            print('PASS: 7 wire tests, 49/46 actual dropped datagrams and all 10 repeats')
    expected = directory / 'wire-summary.json'
    if expected.exists():
        assert sorted(rows, key=lambda row: row['fixture']) == sorted(json.loads(expected.read_text()), key=lambda row: row['fixture'])
        assert len(rows) == 11
        missing = sum(bool(row['missing']) for row in rows)
        assert missing == (2 if directory.name == 'quiche-qlog-failure' else 0)
        print(directory.name, '16-stream connections:', len(rows), 'connections missing FIN:', missing)
print('PASS: all archives, individual files and before/after FIN observations match')
