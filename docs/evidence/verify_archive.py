"""Verify a downloaded GitHub artifact and every extracted evidence byte."""
import hashlib
import json
import zipfile


def verify_archive(directory, expected_commit):
    artifact = json.loads((directory / 'artifact.json').read_text())
    manifest = json.loads((directory / 'manifest.json').read_text())
    raw = (directory / 'raw.zip').read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    assert artifact['workflow_run']['head_sha'] == expected_commit
    assert len(raw) == artifact['size_in_bytes'] == manifest['archive_bytes']
    assert 'sha256:' + digest == artifact['digest'] and digest == manifest['archive_sha256']
    with zipfile.ZipFile(directory / 'raw.zip') as package:
        entries = {row['path']: row for row in manifest['files']}
        assert len(entries) == len(manifest['files'])
        assert {row.filename for row in package.infolist() if not row.is_dir()} == entries.keys()
        for name, expected in entries.items():
            content = package.read(name)
            assert len(content) == expected['bytes']
            assert hashlib.sha256(content).hexdigest() == expected['sha256']
            assert (directory / 'extracted' / name).read_bytes() == content
    return directory / 'extracted'
