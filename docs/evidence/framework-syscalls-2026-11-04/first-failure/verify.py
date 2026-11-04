#!/usr/bin/env python3
"""Retain the failed four-target attempt and verify only its four completed cells."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1]))
from verify_archive import verify_archive

def module(name):
    spec = importlib.util.spec_from_file_location(name, HERE / (name + '.py'))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('download', type=Path)
args = parser.parse_args()
source = 'a45ac544bb8b32efa56429f3976fc3db38764fde'
for name in ['artifact.json', 'manifest.json']:
    assert json.loads((args.download / name).read_text()) == json.loads((HERE / name).read_text())
root = verify_archive(args.download, source)
suite = json.loads((root / 'suite.json').read_text())
assert suite == json.loads((HERE / 'suite.json').read_text())
assert suite['source'] == source
assert [(r['round'], r['target'], r['exit']) for r in suite['runs']] == [(0, 'edge', 0), (0, 'pingora', 0), (0, 'nginx', 1)]
http, trace = module('http_verify'), module('syscall_profile')
successful = 0
for target in ['edge', 'pingora']:
    path = root / f'round-0-{target}/manifest.json'
    rows = http.validate(path, root)
    assert len(rows) == 2 and all(row['status'] == 'passed' and row['failed'] == 0 for row in rows)
    assert {(row['protocol'], row['cache']) for row in rows} == {('h1', 'off'), ('h2', 'off')}
    for row in rows:
        directory = path.parent / (row['stem'] + '.trace')
        report = json.loads((directory / 'syscall-summary.json').read_text())
        parsed = trace.parse_summary((directory / 'syscall-summary.log').read_text())
        assert parsed['total'] == report['total'] and parsed['syscalls'] == report['syscalls']
        assert report['completed'] and report['attached_before']['threads'] and report['attached_after']['threads']
        assert report['detached_threads']
        assert json.loads((directory / 'sidecar-exit.json').read_text())['exit'] == 0
        load = json.loads((directory / 'load-ended.json').read_text())
        assert load['completed']
        assert report['attached_before']['monotonic_ns'] <= load['started_ns'] <= load['ended_ns'] <= report['attached_after']['monotonic_ns'] <= report['stop_requested_monotonic_ns'] <= report['stopped_monotonic_ns']
        successful += row['successful']
nginx = json.loads((root / 'round-0-nginx/manifest.json').read_text())
assert len(nginx['cases']) == 2 and all(row['status'] == 'failed' for row in nginx['cases'])
log = (root / '0-nginx.driver.log').read_text()
assert 'sha256sum' in log and '/proc/7/exe' in log and 'non-zero exit status 1' in log
assert not list((root / 'round-0-nginx').glob('*.trace'))
assert not (root / 'verified-syscalls.json').exists()
print(json.dumps(dict(source=source, completed_cells=4, successful=successful,
    failed_nginx_cells=2, entire_four_target_run_passed=False,
    scope='partial ptrace counts; no four-product result, no executable-byte verification or capacity ranking')))
