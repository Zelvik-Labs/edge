#!/usr/bin/env python3
"""Restore the exact executable that reproduced the native fresh errors."""
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile

ROOT = Path.cwd()
SOURCE = '82e3cf0db722497fb594acdcfabc645a6deb2b56'
ARCHIVE_SHA = '2045fa18ed0085a1092208de2ba359b5ce977289673137e60883d3006a168078'
EXPECTED = {'Zelvik-Edge': '0ba4c5d56b52540f327a55478bd036a7e9516c72933aea4f5aa2da69f09b6aa6',
            'issue34-controller': '1a85adaec923f45c490b2bcc949adb35c562241e01024b3020aae1c99486b6b2'}


def main():
    base = 'repos/Zelvik-Labs/edge/actions/artifacts/13271188885'
    artifact = json.loads(subprocess.check_output(['gh', 'api', base]))
    assert artifact['workflow_run']['id'] == 41714149785 and artifact['workflow_run']['head_sha'] == SOURCE
    assert artifact['digest'] == 'sha256:' + ARCHIVE_SHA
    private = ROOT / '.local/quinn-diagnostic-private'
    private.mkdir(parents=True, exist_ok=True)
    archive = private / 'original-executables.zip'
    with archive.open('wb') as file:
        subprocess.run(['gh', 'api', base + '/zip'], stdout=file, check=True)
    raw = archive.read_bytes()
    assert len(raw) == artifact['size_in_bytes'] and hashlib.sha256(raw).hexdigest() == ARCHIVE_SHA
    binaries = ROOT / '.local/quinn-diagnostic-binaries'
    binaries.mkdir()
    with zipfile.ZipFile(archive) as package:
        expected = {'quinn-gso-binaries/' + name for name in EXPECTED}
        expected.add('quinn-gso-private/protocol-state-client')
        assert {row.filename for row in package.infolist() if not row.is_dir()} == expected
        for name, digest in EXPECTED.items():
            content = package.read('quinn-gso-binaries/' + name)
            assert hashlib.sha256(content).hexdigest() == digest
            path = binaries / name
            path.write_bytes(content)
            path.chmod(0o755)
    (private / 'binary-artifact.json').write_text(json.dumps(artifact, indent=2) + '\n')
    (private / 'build-metadata.json').write_text(json.dumps({'runtime_source': SOURCE,
        'binaries': EXPECTED, 'archive_sha256': ARCHIVE_SHA, 'build': 'reuse exact original executables; no Rust compilation'}, indent=2) + '\n')
    print(json.dumps({'runtime_source': SOURCE, 'binaries': EXPECTED}))


if __name__ == '__main__':
    main()
