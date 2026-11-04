#!/usr/bin/env python3
"""Verify retained diagnostic failures without promoting partial data to a pass."""
import argparse
import importlib.util
import json
from pathlib import Path
from verify_archive import verify_archive


def module(path):
    spec = importlib.util.spec_from_file_location(path.stem, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('frozen', type=Path)
    parser.add_argument('download', type=Path)
    args = parser.parse_args()
    expected = json.loads((args.frozen / 'verification.json').read_text())
    for name in ['artifact.json', 'manifest.json']:
        assert json.loads((args.download / name).read_text()) == json.loads((args.frozen / name).read_text())
    root = verify_archive(args.download, expected['source'])
    assert json.loads((root / 'suite.json').read_text()) == json.loads((args.frozen / 'suite.json').read_text())
    suite = json.loads((root / 'suite.json').read_text())
    assert suite['runs'][-1]['exit'] != 0 and not expected['entire_run_passed']
    observed, rows = [], []
    for path in sorted(root.glob('round-*/manifest.json')):
        data = json.loads(path.read_text())
        observed.append(dict(path=str(path.relative_to(root)), status=data.get('status'),
            cases=[dict(stem=c['stem'], status=c.get('status'), successful=c.get('native', {}).get('successful'),
                        failed=c.get('native', {}).get('failed'), error=c.get('error')) for c in data['cases']]))
        if data.get('status') == 'completed':
            rows += module(args.frozen / 'http_verify.py').validate(path, root)
    assert observed == expected['runs']
    stage = expected['stage']
    if stage == 'per-call-failure':
        assert len(rows) == 2 and sum(r['successful'] for r in rows) == 0 and sum(r['failed'] for r in rows) == 96
        assert all(r['status'] == 'traffic-errors' for r in rows)
        helper = module(args.frozen / 'stack_helper.py')
        for row in rows:
            trace = root / Path(row['manifest']).parent / (row['stem'] + '.trace')
            report = json.loads((trace / 'futex-summary.json').read_text())
            events = json.loads((trace / 'futex-events.json').read_text())
            assert all(report[key] == value for key, value in helper.summarize(events).items())
            assert not report['raw_arguments_published'] and report['completed']
        assert not list(root.rglob('private-futex-raw*')) and not (root / 'verified-futex.json').exists()
    elif stage == 'sampled-first-failure':
        assert len(rows) == 2 and all(row['status'] == 'failed' for row in rows)
        assert sum(r['successful'] for r in rows) == 93264 and sum(r['failed'] for r in rows) == 103625
        data = json.loads((root / 'round-0-edge/manifest.json').read_text())
        for case in data['cases']:
            assert case['proxy_after']['memory.peak']['bytes'] >= 2**31
            before, after = case['proxy_before']['memory.events'], case['proxy_after']['memory.events']
            assert after['max'] > before['max'] and after['oom_kill'] == before['oom_kill']
            log = (root / 'round-0-edge' / (case['stem'] + '.perf-record.log')).read_text()
            assert 'lost' in log and ' MB ' in log
        assert not (root / 'verified-futex-sampled.json').exists()
    elif stage == 'compatibility-failure':
        assert len(rows) == 2 and sum(r['successful'] for r in rows) == 99992
        assert all(row['status'] == 'passed' and row['failed'] == 0 for row in rows)
        log = (root / '0-pingora.driver.log').read_text()
        assert 'FileNotFoundError' in log and '/proc/1/task/51/status' in log
        failure = json.loads((root / 'round-0-pingora/0000-h2-off-reuse.trace/trace-failure.json').read_text())
        assert not failure['completed'] and 'FileNotFoundError' in failure['error']
        assert not (root / 'verified-syscalls.json').exists()
    elif stage == 'source-checkout-failure':
        assert not observed
        log = (root / '0-edge.driver.log').read_text()
        assert 'a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a' in log
        assert 'protocol_state_client.c' in log and 'exit status 128' in log
    elif stage in ['period-preflight-failure', 'period-format-failure']:
        assert all(row['status'] == 'failed' and row.get('successful') is None for row in rows)
        assert not list(root.glob('round-*' + '/000*.client.tsv*'))
        if stage == 'period-preflight-failure':
            logs = list(root.glob('round-*/*.perf-preflight.log'))
            assert len(logs) == 2
            assert all('valid terms: call-graph,stack-size' in path.read_text() for path in logs)
        else:
            log = (root / '0-edge.driver.log').read_text()
            assert 'effective tracepoint period differs' in log
            text = (root / 'round-0-edge/0000-h2-off-reuse.perf-preflight.txt').read_text()
            assert '{ sample_period, sample_freq }: 512' in text
        assert not (root / 'verified-futex-sampled.json').exists()
    else:
        raise ValueError('unsupported frozen failure kind')
    print(json.dumps(dict(source=expected['source'], stage=stage, result='original failure retained',
        scope='archive/records verified; no successful entire diagnostic run or input executable-byte verification claimed')))


if __name__ == '__main__':
    main()
