#!/usr/bin/env python3
"""Verify successful default gates separately from the retained first CI failure."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('directory', type=Path, help='test.log, check.log, jobs.json and evidence download')
args = parser.parse_args()
expected = json.loads((HERE / 'verification.json').read_text())
jobs = json.loads((args.directory / 'jobs.json').read_text())
assert jobs == json.loads((HERE / 'jobs.json').read_text())
assert jobs['databaseId'] == expected['run'] and jobs['headSha'] == expected['source']
assert jobs['status'] == 'completed' and jobs['conclusion'] == 'success'
for name, description in expected['logs'].items():
    raw = (args.directory / (name + '.log')).read_bytes()
    assert len(raw) == description['bytes'] and hashlib.sha256(raw).hexdigest() == description['sha256']
    assert expected['source'].encode() in raw
test = (args.directory / 'test.log').read_text()
assert '1374 passed; 0 failed; 5 ignored' in test
for regression in ['borrowed_lookup_normalization_preserves_legacy_output_and_ownership',
                   'borrowed_request_lookup_preserves_reparse_and_wildcard_routes',
                   'failed_stream_drains_new_reuse_without_cancelling_an_active_peer']:
    assert regression + ' ... ok' in test
for job in jobs['jobs']:
    if job['name'].startswith(('Test -', 'Check / Clippy')):
        assert job['status'] == 'completed' and job['conclusion'] == 'success'
        steps = {step['name']: step['conclusion'] for step in job['steps']}
        required = ['cargo test', 'cargo test (integration)', 'cargo test (vendored pingora)',
                    'Repeat actual H3 draining and replacement lifecycle',
                    'Build matching full-node XSK validation binaries',
                    'Validate XSK wake modes in isolated full nodes'] if job['name'].startswith('Test -') else [
                    'cargo check (all targets)', 'cargo check (vendored pingora crates)', 'cargo clippy']
        assert all(steps[name] == 'success' for name in required)
subprocess.run([sys.executable, str(HERE / 'verify.py'), str(args.directory / 'evidence'),
                '--commit', expected['source']], check=True)
print('PASS default full CI 1374 library + 16 binary tests and native32; H3 64-attempt artifact verified separately')
