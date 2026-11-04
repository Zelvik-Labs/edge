#!/usr/bin/env python3
"""Sample futex callsites without stopping the tracee on every syscall."""
import argparse
from collections import Counter
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
PRIVATE = ROOT / '.local/futex-sampled-private'
PUBLIC = ROOT / '.local/futex-sampled-ci'
ORDERS = [['edge'], ['edge']]
EVENT = 'syscalls:sys_enter_futex'
PERIOD = 512


def module(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def profile(path):
    blocks = [block for block in path.read_text().split('\n\n') if EVENT + ':' in block]
    assert blocks, 'empty futex tracepoint sample'
    flat = Path(str(path).replace('.stacks.txt', '.perf-flat.txt')).read_text()
    lost = re.search(r'Total Lost Samples:\s*(\d+)', flat)
    assert lost and int(lost[1]) == 0, 'lost samples or missing loss counter'
    operations, chains, stackless = Counter(), Counter(), 0
    for block in blocks:
        lines = block.splitlines()
        header = next(line for line in lines if EVENT + ':' in line)
        assert re.fullmatch(r'.*syscalls:sys_enter_futex: operation=\d+', header)
        operation = int(header.rsplit('=', 1)[1])
        operations[operation] += 1
        frames = tuple(re.sub(r'^\s*[0-9a-f]+\s+', '', line).strip()
                       for line in lines[lines.index(header) + 1:] if line.strip())
        chains[operation, frames] += 1
        stackless += not frames
    return dict(event=EVENT, samples=len(blocks), lost_samples=0, stackless=stackless,
        operations={str(key): value for key, value in operations.items()},
        chains=[dict(operation=op, frames=list(frames), samples=count)
                for (op, frames), count in chains.most_common()],
        sampling_period=PERIOD, sha256=hashlib.sha256(path.read_bytes()).hexdigest())


def run():
    original = module('sampled_framework_inputs', 'scripts/perf/framework-profile/evidence.py')
    original.PRIVATE = PRIVATE
    original.prepare()
    bins = ROOT / '.local/architecture-profile-binaries'
    suite = dict(source=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        orders=ORDERS, runs=[], event=EVENT, period=PERIOD,
        scope='sampled futex callsites of fixed ba474 Edge; raw payload excluded; no CPU, wait time or capacity attribution',
        source_hashes={name: original.sha(ROOT / name) for name in ['scripts/perf/profile_architecture.py',
            'scripts/perf/futex-sampled/evidence.py']})
    (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
    for number, order in enumerate(ORDERS):
        for target in order:
            output = PRIVATE / f'round-{number}-{target}'
            with (PRIVATE / f'{number}-{target}.driver.log').open('w') as log:
                result = subprocess.run(['python3', 'scripts/perf/profile_architecture.py', '--target', target,
                    '--output', str(output), '--tls', str(PRIVATE / 'tls'),
                    '--client', str(bins / '.local/http-state-ci/protocol-state-client'),
                    '--edge-binary', str(bins / 'target/release/Zelvik-Edge'),
                    '--controller', str(bins / 'target/release/examples/issue34-controller'),
                    '--profiler-image', 'edge-architecture-profiler:20261104',
                    '--protocols', 'h1', 'h2', '--states', 'off', '--repeats', '1',
                    '--seconds', '20', '--event', EVENT, '--period', str(PERIOD)],
                    stdout=log, stderr=subprocess.STDOUT)
            suite['runs'].append(dict(round=number, target=target,
                manifest=str(output.relative_to(PRIVATE) / 'manifest.json'), exit=result.returncode))
            (PRIVATE / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
            assert result.returncode == 0, 'futex sampled fixture failed; preserve partial evidence'
    verify()


def verify():
    http = module('sampled_futex_http', 'scripts/perf/http-state/verify.py')
    driver = module('sampled_futex_driver', 'scripts/perf/profile_architecture.py')
    suite = json.loads((PRIVATE / 'suite.json').read_text())
    provenance = json.loads((PRIVATE / 'provenance.json').read_text())
    assert [(run['round'], run['target']) for run in suite['runs']] == [(i, t) for i, order in enumerate(ORDERS) for t in order]
    rows, successful = [], 0
    for run in suite['runs']:
        assert run['exit'] == 0
        path = PRIVATE / run['manifest']
        data = json.loads(path.read_text())
        assert data['source_revision'] == suite['source'] and data['args']['profile_event'] == EVENT
        assert data['args']['profile_period'] == PERIOD
        assert data['client_sha256'] == provenance['expected_binaries']['.local/http-state-ci/protocol-state-client']
        assert data['client_source_revision'] == 'a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a'
        checked = http.validate(path, PRIVATE)
        assert len(checked) == 2 and all(row['status'] == 'passed' and row['failed'] == 0 for row in checked)
        assert {(r['protocol'], r['cache']) for r in checked} == {('h1', 'off'), ('h2', 'off')}
        successful += sum(row['successful'] for row in checked)
        for case in data['cases']:
            stem = path.parent / case['stem']
            identity = json.loads(Path(str(stem) + '.profile-target.json').read_text())
            assert identity['executable_sha256'] == provenance['expected_binaries']['target/release/Zelvik-Edge']
            row = profile(Path(str(stem) + '.stacks.txt'))
            row.update(round=run['round'], protocol=case['protocol'])
            command = json.loads(Path(str(stem) + '.perf-command.json').read_text())
            assert command[command.index('-e') + 1] == EVENT
            assert command.index('-c') > command.index('-e')
            assert command[command.index('-c') + 1] == str(PERIOD)
            for suffix in ['.perf-preflight.txt', '.perf-effective.txt']:
                driver.verify_effective_period(Path(str(stem) + suffix).read_text(), PERIOD)
            rows.append(row)
    assert len(rows) == 4
    summary = dict(source=suite['source'], scope=suite['scope'], cells=len(rows),
        successful=successful, failed=0, samples=sum(r['samples'] for r in rows), lost_samples=0,
        records=rows, no_complete_measurement_window_or_exact_per_response_counts_claimed=True)
    (PRIVATE / 'verified-futex-sampled.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k != 'records'}))


def collect():
    original = module('sampled_futex_collector', 'scripts/perf/framework-profile/evidence.py')
    original.PRIVATE, original.PUBLIC = PRIVATE, PUBLIC
    original.collect()
    path = PUBLIC / 'retained-manifest.json'
    manifest = json.loads(path.read_text())
    for source in PRIVATE.rglob('*'):
        if source.is_file() and (source.name == 'verified-futex-sampled.json' or source.name.endswith(('.perf-preflight.txt', '.perf-effective.txt', '.perf-preflight.log'))):
            relative = source.relative_to(PRIVATE)
            output = PUBLIC / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, output)
            manifest['files'].append(dict(path=str(relative), bytes=source.stat().st_size, sha256=original.sha(source)))
    path.write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['run', 'verify', 'collect'])
    parser.add_argument('--directory', type=Path)
    args = parser.parse_args()
    if args.directory:
        PRIVATE = args.directory.resolve()
    globals()[args.mode]()
