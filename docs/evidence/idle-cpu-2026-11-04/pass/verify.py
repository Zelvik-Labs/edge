#!/usr/bin/env python3
"""Replay original CPU/wire records without executing any measured binary."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

SOURCE = 'd07a4c7b29fc93e164282c3669fb9d8b3afe38a6'
FROZEN = Path(__file__).resolve().parent
REPO = FROZEN.parents[3]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(download, header_download):
    archive = module('idle_archive', FROZEN.parents[1] / 'verify_archive.py')
    original = archive.verify_archive(download, SOURCE)
    header = archive.verify_archive(header_download, '4171ea700386d8ed7c755255c7eeccb7e804d644')
    expected = json.loads((original / 'build-metadata.json').read_text())
    assert expected['source'] == SOURCE
    for name, digest in expected['sources'].items():
        assert sha(original / 'source' / name) == digest
        raw = subprocess.check_output(['git', 'show', SOURCE + ':scripts/perf/idle-cpu/' + name], cwd=REPO)
        assert hashlib.sha256(raw).hexdigest() == digest
    raw = subprocess.check_output(['git', 'show', SOURCE + ':scripts/perf/http-state/verify.py'], cwd=REPO)
    assert hashlib.sha256(raw).hexdigest() == sha(FROZEN / 'http_verify.py')
    for name in ['Zelvik-Edge', 'issue34-controller']:
        assert sha(header / 'cache-range-binaries/after' / name) == expected['expected_inputs'][name]
    assert sha(header / 'cache-range-private/protocol-state-client') == expected['expected_inputs']['protocol-state-client']
    assert sha(original / 'bare-pingora-proxy') == expected['expected_inputs']['bare-pingora-proxy'] == '1a5439941c53dc32044b665013add230ac8a2062ab34e72f92dfde7799d093c2'
    with tempfile.TemporaryDirectory() as directory:
        work = Path(directory) / 'records'
        shutil.copytree(original, work)
        suite = module('idle_original_suite', original / 'source/suite.py')
        suite.ROOT, suite.OUT, suite.PRIVATE = REPO, work, work / 'records'

        def frozen_module(name, path):
            if path.name == 'observer.py':
                return module(name, original / 'source/observer.py')
            assert path == REPO / 'scripts/perf/http-state/verify.py'
            return module(name, FROZEN / 'http_verify.py')

        suite.module = frozen_module
        suite.verify()
        for name in ['verified-summary.json', 'verified-records.json']:
            assert json.loads((work / name).read_text()) == json.loads((original / name).read_text())
    return json.loads((original / 'verified-summary.json').read_text())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('download', type=Path)
    parser.add_argument('header_download', type=Path)
    args = parser.parse_args()
    summary = verify(args.download.resolve(), args.header_download.resolve())
    print(json.dumps(dict(raw_archive_replayed=True, **summary)))
