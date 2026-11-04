#!/usr/bin/env python3
"""Replay original transfers and controls against the original measured binaries."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

SOURCE = 'ebd34770a817b68bd833f876918fc660a0089f49'
BEFORE = '4613a6b8d468be8df646f7cb628a42b89bfe0e98'
AFTER = '60cf30ced5f85f68b84f27e170972ca528115434'
FROZEN = Path(__file__).resolve().parent
REPO = FROZEN.parents[3]
RUNTIME = ['pingora-main/pingora-core/src/protocols/l4/stream.rs',
           'src/http_proxy_manager.rs', 'src/memory_governor.rs']


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_sha(revision, path):
    raw = subprocess.check_output(['git', 'show', revision + ':' + path], cwd=REPO)
    return hashlib.sha256(raw).hexdigest()


def replay(download, binary_download):
    archive = module('buffer_archive', FROZEN.parents[1] / 'verify_archive.py')
    original = archive.verify_archive(download, SOURCE)
    binaries = archive.verify_archive(binary_download, SOURCE)
    build = json.loads((original / 'build-metadata.json').read_text())
    suite = json.loads((original / 'suite.json').read_text())
    assert build['harness_source'] == suite['source'] == SOURCE
    assert build['baseline'] == BEFORE and build['runtime_changes'] == RUNTIME
    assert build['builds_complete_before_measurement']
    changed = subprocess.check_output(
        ['git', 'diff', '--name-only', BEFORE, AFTER], cwd=REPO, text=True).splitlines()
    runtime = [p for p in changed if p.startswith(('src/', 'crates/', 'vendor/', 'pingora-main/'))
               or p in ['Cargo.toml', 'Cargo.lock', 'build.rs', 'rust-toolchain.toml', '.cargo/config.toml']]
    assert sorted(runtime) == RUNTIME
    for variant, revision in [('before', BEFORE), ('after', AFTER)]:
        data = build['variants'][variant]
        assert data['source'] == revision
        assert set(data['binaries']) == {'Zelvik-Edge', 'issue34-controller'}
        assert set(data['source_hashes']) == set(RUNTIME + ['Cargo.toml', 'Cargo.lock'])
        for path, digest in data['source_hashes'].items():
            assert source_sha(revision, path) == digest
    for path, digest in suite['sources'].items():
        assert source_sha(SOURCE, path) == digest
    assert sha(binaries / 'cache-range-private/protocol-state-client') == suite['client_sha256']
    for name, source in [('verify.py', 'scripts/perf/downstream-buffer/verify.py'),
                         ('http_verify.py', 'scripts/perf/http-state/verify.py')]:
        assert sha(FROZEN / 'source' / name) == source_sha(SOURCE, source)
    with tempfile.TemporaryDirectory() as directory:
        temporary = Path(directory)
        work = temporary / 'records'
        shutil.copytree(original, work)
        http = temporary / 'scripts/perf/http-state/verify.py'
        http.parent.mkdir(parents=True)
        shutil.copy2(FROZEN / 'source/http_verify.py', http)
        verifier = module('buffer_original_verify', FROZEN / 'source/verify.py')
        verifier.ROOT = temporary
        verifier.verify(work, binaries / 'cache-range-binaries')
        for name in ['verified-records.json', 'verified-summary.json']:
            assert json.loads((work / name).read_text()) == json.loads((original / name).read_text())
    return json.loads((original / 'verified-summary.json').read_text())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('download', type=Path)
    parser.add_argument('binary_download', type=Path)
    args = parser.parse_args()
    result = replay(args.download.resolve(), args.binary_download.resolve())
    print(json.dumps(dict(raw_archive_replayed=True,
                          **{k: v for k, v in result.items() if k != 'comparisons'})))
