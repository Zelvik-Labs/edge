#!/usr/bin/env python3
"""Restore the original measured variants, verifying GitHub and binary hashes."""
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile

ARTIFACT = 13269688187
SOURCE = '0cf22db48e2e6a64c34f82599b6285d0efc90de5'
ARCHIVE_SHA = '96b53b7d1cf4737fde0611d5a80d846fa8b51bfd5fc7a83d1a17330ff0556ec1'
ROOT = Path.cwd()


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def main():
    base = f'repos/Zelvik-Labs/edge/actions/artifacts/{ARTIFACT}'
    artifact = json.loads(subprocess.check_output(['gh', 'api', base]))
    assert artifact['workflow_run']['id'] == 41712678098
    assert artifact['workflow_run']['head_sha'] == SOURCE
    assert artifact['digest'] == 'sha256:' + ARCHIVE_SHA
    directory = ROOT / '.local/cache-range-private'
    directory.mkdir(parents=True, exist_ok=True)
    archive = directory / 'binary-archive.zip'
    with archive.open('wb') as output:
        subprocess.run(['gh', 'api', base + '/zip'], stdout=output, check=True)
    raw = archive.read_bytes()
    assert len(raw) == artifact['size_in_bytes'] and sha(raw) == ARCHIVE_SHA
    expected = {'cache-range-binaries/build-metadata.json', 'cache-range-private/protocol-state-client'}
    expected.update(f'cache-range-binaries/{variant}/{name}' for variant in ['before', 'after']
                    for name in ['Zelvik-Edge', 'issue34-controller'])
    with zipfile.ZipFile(archive) as package:
        assert {item.filename for item in package.infolist() if not item.is_dir()} == expected
        metadata = json.loads(package.read('cache-range-binaries/build-metadata.json'))
        assert metadata['builds_complete_before_measurement']
        assert metadata['variants']['before']['source'] == 'a4e95a239abb82420f65d79f813654ba67fedad9'
        assert metadata['variants']['after']['source'] == SOURCE
        for variant, data in metadata['variants'].items():
            for name, details in data['binaries'].items():
                content = package.read(f'cache-range-binaries/{variant}/{name}')
                assert len(content) == details['bytes'] and sha(content) == details['sha256']
        for name in sorted(expected):
            path = ROOT / '.local' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(package.read(name))
            path.chmod(0o644 if path.suffix == '.json' else 0o755)
    (directory / 'build-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    (directory / 'binary-artifact.json').write_text(json.dumps(artifact, indent=2) + '\n')
    print(json.dumps({'artifact': ARTIFACT, 'archive_sha256': ARCHIVE_SHA,
                      'variant_sources': {key: value['source'] for key, value in metadata['variants'].items()}}))


if __name__ == '__main__':
    main()
