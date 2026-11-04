"""Restore byte-identical Actions evidence after verifying every retained hash."""
import hashlib
import json
from pathlib import Path
import zipfile


def sha(data):
    return hashlib.sha256(data).hexdigest()


for root in [Path('wire')]:
    artifact = json.loads((root / 'artifact.json').read_text())
    manifest = json.loads((root / 'manifest.json').read_text())
    archive = (root / 'raw.zip').read_bytes()
    assert len(archive) == artifact['size_in_bytes'] == manifest['archive_bytes']
    assert 'sha256:' + sha(archive) == artifact['digest']
    assert sha(archive) == manifest['archive_sha256']
    entries = {item['path']: item for item in manifest['files']}
    assert len(entries) == len(manifest['files'])
    base = (root / 'extracted').resolve()
    base.mkdir(exist_ok=True)
    with zipfile.ZipFile(root / 'raw.zip') as package:
        assert len(package.namelist()) == len(entries)
        assert set(package.namelist()) == set(entries)
        for name, item in entries.items():
            target = base / name
            assert target.resolve().is_relative_to(base)
            content = package.read(name)
            assert len(content) == item['bytes'] and sha(content) == item['sha256']
            if target.exists():
                assert target.read_bytes() == content
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(content)
    actual = {str(path.relative_to(base)) for path in base.rglob('*') if path.is_file()}
    assert actual == set(entries)
    print(f'{root}: verified {len(entries)} original files')
