"""Independently verify the seven real H3 origin regression records and archive."""
import hashlib
import json
from pathlib import Path
import sys
import zipfile


def sha(data):
    return hashlib.sha256(data).hexdigest()


root, output = map(Path, sys.argv[1:3])
source = sys.argv[3]
artifact = json.loads((root / 'artifact.json').read_text())
manifest = json.loads((root / 'manifest.json').read_text())
archive = (root / 'raw.zip').read_bytes()
assert artifact['workflow_run']['head_sha'] == source
assert artifact['workflow_run']['id'] == 41623661117
assert len(archive) == artifact['size_in_bytes'] == manifest['archive_bytes']
assert 'sha256:' + sha(archive) == artifact['digest']
assert sha(archive) == manifest['archive_sha256']
expected = {
    'abandoned_request_body_writer_cannot_be_finished_into_a_healthy_pool_entry': (2, 2),
    'idle_expiration_releases_the_sender_driver_and_endpoint': (1, 1),
    'partial_response_and_reset_are_not_returned_to_the_idle_pool': (2, 2),
    'pending_header_and_trailer_timeouts_do_not_prove_reusable_completion': (2, 2),
    'pooled_h3_contexts_keep_peer_trust_and_group_isolation': (1, 1),
    'request_upload_progresses_while_response_headers_are_pending': (1, 1),
    'sequential_h3_requests_reuse_one_protocol_driver_and_quic_connection': (12, 1),
}
assert len(manifest['files']) == len(expected) == 7
records = {}
with zipfile.ZipFile(root / 'raw.zip') as package:
    assert len(package.infolist()) == 7
    for item in manifest['files']:
        name = item['path']
        raw = (root / 'extracted' / name).read_bytes()
        assert package.read(name) == raw
        assert len(raw) == item['bytes'] and sha(raw) == item['sha256']
        row = json.loads(raw)
        test = row['test'].removeprefix('origin_h3::wire_tests::')
        assert test in expected and test not in records
        assert row['passed'] is True
        requests, connections = expected[test]
        assert row['resolved_h3_requests'] == requests
        assert row['accepted_quic_connections'] == connections
        assert row['closed_quic_connections'] == connections
        assert name == row['test'].replace('::', '__') + '.json'
        records[test] = row
assert set(records) == set(expected)
result = {
    'passed': True,
    'source_commit': source,
    'artifact_id': artifact['id'],
    'archive_sha256': sha(archive),
    'wire_cases': len(records),
    'resolved_h3_requests': sum(row['resolved_h3_requests'] for row in records.values()),
    'accepted_quic_connections': sum(row['accepted_quic_connections'] for row in records.values()),
    'closed_quic_connections': sum(row['closed_quic_connections'] for row in records.values()),
    'sequential_requests_on_one_quic_and_driver': 12,
    'scope': 'Kernel-loopback H3 origin correctness; six abnormal/aborted/timeout controls are included in 21 resolved requests, not 21 successful responses; no throughput, RSS or real-NIC AF_XDP claim',
    'cases': records,
}
output.write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps({key: value for key, value in result.items() if key != 'cases'}))
