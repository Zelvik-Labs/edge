"""Recompute the close pattern of this immutable same-client group run."""
from collections import Counter
import json
from pathlib import Path
import re


def verify(root):
    suite = json.loads((root / 'suite.json').read_text())
    counts = Counter()
    traces = []
    for run in suite['runs']:
        path = root / run['manifest']
        manifest = json.loads(path.read_text())
        case = manifest['cases'][0]
        for shard in range(2):
            text = (path.parent / (case['stem'] + f'.{shard}.stderr')).read_text()
            blocks = re.findall(r'quic_trace_begin connection=(-?\d+) dropped_bytes=(\d+) close_frames=(\d+)\n(.*?)\nquic_trace_end connection=\1', text, re.S)
            assert len(blocks) == len(re.findall(r'^transfer_error ', text, re.M))
            for connection, dropped, closes, body in blocks:
                reason = re.findall(r'quic_close direction=(\w+) transport=(\d+) code=(\d+) frame=(\d+) reason_bytes=(\d+) reason_hex=([0-9a-f]*)', body)
                assert reason == [('write', '1', '14', '0', '28', '5258206b65792075706461746520616761696e20746f6f20736f6f6e')]
                assert int(dropped) == 0 and int(closes) == 1
                handshake_done = bool(re.search(r'quic_frame direction=read kind=full type=30 ', body))
                headers = [{'phase': int(p), 'pn_truncated': int(n), 'pn_bytes': int(s)}
                           for p, n, s in re.findall(r'quic_packet_header direction=read short=1 key_phase=(\d+) pn_truncated=(\d+) pn_bytes=(\d+)', body)]
                phase_one = [h for h in headers if h['phase'] == 1]
                assert not handshake_done and phase_one
                assert any(h['phase'] == 0 for h in headers) and headers[-1] == phase_one[-1]
                assert all(h['pn_bytes'] == 1 for h in phase_one)
                assert phase_one[0]['pn_truncated'] in [10, 11, 12]
                groups = [int(g) for g in re.findall(r'quic_tls_key_share_group id=(\d+) authenticated=false', body)]
                assert groups == [29 if run['client'] == 'x25519' else 4588]
                counts[run['client']] += 1
                traces.append({'client': run['client'], 'round': run['round'], 'shard': shard,
                               'connection': int(connection), 'dropped_trace_bytes': int(dropped),
                               'close_code': 14, 'close_reason': bytes.fromhex(reason[0][-1]).decode(),
                               'observed_group': groups[0], 'received_handshake_done_before_close': handshake_done,
                               'first_phase_one_header': phase_one[0],
                               'all_phase_one_headers': phase_one,
                               'header_scope': 'unprotected metadata before authentication; not proof of an authenticated peer violation'})
    summary = json.loads((root / 'verified-summary.json').read_text())
    assert dict(counts) == {c: r['failed'] for c, r in summary['clients'].items()}
    pattern = {'source': suite['source'], 'traces': len(traces), 'errors_by_client': dict(counts),
               'phase_one_packet_numbers': dict(Counter(t['first_phase_one_header']['pn_truncated'] for t in traces)),
               'received_handshake_done_before_close': sum(t['received_handshake_done_before_close'] for t in traces),
               'dropped_trace_bytes': sum(t['dropped_trace_bytes'] for t in traces),
               'scope': 'all retained local KEY_UPDATE_ERROR traces; hybrid group is not necessary for reproduction; no determination of the violating endpoint'}
    (root / 'verified-traces.json').write_text(json.dumps(traces, indent=2) + '\n')
    (root / 'verified-pattern.json').write_text(json.dumps(pattern, indent=2) + '\n')
    print(json.dumps(pattern))
