#!/usr/bin/env python3
"""Keep the original library CI failure and its exact log identity."""
import argparse, hashlib, json
from pathlib import Path
HERE = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('test_log', type=Path)
args = parser.parse_args()
expected = json.loads((HERE / 'verification.json').read_text())
raw = args.test_log.read_bytes()
assert len(raw) == expected['log']['bytes'] and hashlib.sha256(raw).hexdigest() == expected['log']['sha256']
text = raw.decode()
assert expected['source'] in text and '1373 passed; 1 failed; 5 ignored' in text
assert expected['failed_test'] in text and expected['failure_location'] in text
assert 'Remote error: ApplicationClose: H3_NO_ERROR' in text
for name in ['borrowed_lookup_normalization_preserves_legacy_output_and_ownership', 'borrowed_request_lookup_preserves_reparse_and_wildcard_routes']:
    assert name + ' ... ok' in text
print('PASS original full CI failure retained, both new configuration regressions passed; native skipped')
