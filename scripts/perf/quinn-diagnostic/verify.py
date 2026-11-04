#!/usr/bin/env python3
"""Recompute error counts, prove hooks ran and decode retained close frames."""
import argparse
from collections import Counter
import hashlib
import importlib.util
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[3]


def verify(root, binaries):
    spec = importlib.util.spec_from_file_location('diagnostic_verify', ROOT / 'scripts/perf/http-state/verify.py')
    http = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(http)
    suite = json.loads((root / 'suite.json').read_text())
    client = binaries.parent / 'quinn-diagnostic-private/protocol-state-client'
    assert hashlib.sha256(client.read_bytes()).hexdigest() == suite['client_sha256']
    for name, digest in suite['build']['binaries'].items():
        assert hashlib.sha256((binaries / name).read_bytes()).hexdigest() == digest
    assert len(suite['runs']) == 4
    assert [(r['round'], r['backend']) for r in suite['runs']] == [
        (0, 'quinn-native'), (0, 'quinn-shared'), (1, 'quinn-shared'), (1, 'quinn-native')]
    rows, traces, partial = [], [], []
    infrastructure = []
    for run in suite['runs']:
        if run['infrastructure_error']:
            infrastructure.append(run)
            continue
        path = root / run['manifest']
        manifest = json.loads(path.read_text())
        assert manifest['source_revision'] == suite['source'] and manifest['client_sha256'] == suite['client_sha256']
        assert manifest['args']['edge_backend'] == run['backend']
        if run['backend'] == 'quinn-native':
            assert run['native_listener_startup'] and all('gso=Auto' in line for line in run['native_listener_startup'])
        else:
            assert not run['native_listener_startup']
        assert manifest['args']['transport_profile'] == 'matched' and manifest['args']['address_validation'] == 'off'
        assert manifest['origin_readiness'] and not manifest['origin_reference']['failed']
        records = http.validate(path, root)
        assert len(records) == 1
        row = records[0]
        assert row['payload_bytes'] == 32768 and row['protocol'] == 'h3' and row['cache'] == 'hit' and row['connection'] == 'fresh'
        limit = manifest['args']['fresh_requests']
        if 'request_limit' in run:
            assert limit == run['request_limit'] == (16384 if run['backend'] == 'quinn-native' else 64)
        else:
            assert limit == 16384  # Retain the original timed-out control run.
        row.update(round=run['round'], backend=run['backend'])
        rows.append(row)
        case = manifest['cases'][0]
        complete = case.get('native') is not None
        if complete:
            assert row.get('successful', 0) + row.get('failed', 0) == limit
        else:
            assert row['status'] == 'failed' and row['error']
        for shard in range(2):
            text = (path.parent / (case['stem'] + f'.{shard}.stderr')).read_text()
            hooks = re.findall(r'quic_diagnostic_summary contexts=(\d+) messages=(\d+) frames=(\d+) close_frames=(\d+)', text)
            assert len(hooks) <= 1 and (hooks or not complete)
            if hooks:
                contexts, messages, frames, _ = map(int, hooks[0])
                assert contexts >= limit // 2 and messages > 0 and frames > 0, hooks
            if not complete:
                raw = path.parent / (case['stem'] + f'.{shard}.client.tsv')
                lines = raw.read_text().splitlines(keepends=True) if raw.exists() else []
                trailing_bytes = 0
                if lines and not lines[-1].endswith('\n'):
                    trailing_bytes = len(lines.pop().encode())
                seen = []
                for line in lines:
                    fields = line.strip().split('\t')
                    assert len(fields) == 8
                    code, version, connects, received, seconds, first, error, connection = fields
                    int(connects), float(seconds), float(first), int(connection)
                    seen.append((int(code), version, int(received), int(error)))
                partial.append({'backend': run['backend'], 'round': run['round'], 'shard': shard,
                    'retained_records': len(seen), 'valid_records': sum(c == 200 and v == '3' and b == 32768 and e == 0 for c, v, b, e in seen),
                    'error_distribution': dict(Counter(f'curl_{e}/http_{c}/v_{v}' for c, v, _, e in seen if c != 200 or v != '3' or e)),
                    'trailing_partial_bytes': trailing_bytes, 'process_summary_present': bool(hooks),
                    'scope': 'incomplete killed process; excluded from complete counts and all capacity comparisons'})
            blocks = re.findall(r'quic_trace_begin connection=(-?\d+) dropped_bytes=(\d+) close_frames=(\d+)\n(.*?)\nquic_trace_end connection=\1', text, re.S)
            errors = re.findall(r'^transfer_error ', text, re.M)
            assert len(blocks) == len(errors)
            for connection, dropped, closes, content in blocks:
                reasons = []
                for match in re.finditer(r'quic_close direction=(\w+) transport=(\d+) code=(\d+) frame=(\d+) reason_bytes=(\d+) reason_hex=([0-9a-f]*)', content):
                    direction, transport, code, frame, length, raw = match.groups()
                    reason = bytes.fromhex(raw)
                    assert len(reason) == min(256, int(length))
                    reasons.append({'direction': direction, 'transport': bool(int(transport)),
                        'code': int(code), 'frame': int(frame), 'reason': reason.decode('utf-8', errors='replace'),
                        'reason_bytes': int(length), 'reason_hex': raw, 'truncated': len(reason) < int(length)})
                traces.append({'backend': run['backend'], 'round': run['round'], 'shard': shard,
                    'connection': int(connection), 'dropped_trace_bytes': int(dropped),
                    'close_frame_count': int(closes), 'closes': reasons,
                    'received_handshake_done_before_close': bool(re.search(r'quic_frame direction=read kind=full type=30 ', content)),
                    'unverified_received_short_headers': [
                        {'key_phase': int(phase), 'pn_truncated': int(pn), 'pn_bytes': int(size)}
                        for phase, pn, size in re.findall(
                            r'quic_packet_header direction=read short=1 key_phase=(\d+) pn_truncated=(\d+) pn_bytes=(\d+)', content)]})
    summary = {'source': suite['source'], 'runtime_source': suite['build']['runtime_source'],
        'scope': suite['scope'], 'cases': len(rows), 'statuses': dict(Counter(r['status'] for r in rows)),
        'successful': sum(r.get('successful', 0) for r in rows), 'failed': sum(r.get('failed', 0) for r in rows),
        'infrastructure_failures': infrastructure, 'error_traces': len(traces),
        'incomplete_processes': partial,
        'traces_with_close_frame': sum(bool(t['closes']) for t in traces),
        'close_reasons': dict(Counter(json.dumps(c, sort_keys=True) for t in traces for c in t['closes']))}
    (root / 'verified-records.json').write_text(json.dumps(rows, indent=2) + '\n')
    (root / 'verified-traces.json').write_text(json.dumps(traces, indent=2) + '\n')
    (root / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('binaries', type=Path)
    args = parser.parse_args()
    verify(args.root, args.binaries)
