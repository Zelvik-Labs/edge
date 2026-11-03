#!/usr/bin/env python3
"""Run the already compiled configuration benchmark on one CPU; retain raw data."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cpu_snapshot():
    return {parts[0]: list(map(int, parts[1:]))
            for line in Path('/proc/stat').read_text().splitlines()
            if (parts := line.split()) and parts[0].startswith('cpu')}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build-messages', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    executables = set()
    profiles = []
    for line in args.build_messages.read_text().splitlines():
        message = json.loads(line)
        if (message.get('reason') == 'compiler-artifact'
                and message['target']['name'] == 'udp_demux_bench'
                and message.get('executable')):
            executables.add(Path(message['executable']).resolve(strict=True))
            profiles.append(message['profile'])
    assert len(executables) == 1, executables
    binary = executables.pop()
    for process in Path('/proc').glob('[0-9]*'):
        try:
            if (process/'comm').read_text().strip() in {'rustc', 'mbx-rustc', 'cc1', 'cc1plus', 'clang', 'ld.lld'}:
                state = next(line for line in (process/'status').read_text().splitlines() if line.startswith('State:'))
                assert state.split()[1] == 'Z', 'benchmark cannot overlap compilation'
        except FileNotFoundError:
            continue
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    sources = list(map(Path, [
        'Cargo.toml', 'Cargo.lock', 'src/config.rs', 'src/config_models.rs',
        'src/quic_udp_demux.rs', 'src/http3_proxy_manager.rs',
        'benches/udp_demux_bench.rs', '.github/workflows/ci.yml', __file__,
    ]))
    hashes = {str(path): digest(path) for path in sources}
    cpus = sorted(os.sched_getaffinity(0))
    command = ['taskset', '-c', str(cpus[0]), str(binary), '--bench', '--noplot']
    metadata = {
        'source_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'dirty_files': subprocess.check_output(['git', 'status', '--porcelain'], text=True).splitlines(),
        'binaries': {str(binary): digest(binary)}, 'sources': hashes,
        'command': command, 'available_cpus': cpus, 'benchmark_cpu': cpus[0],
        'build_profiles': profiles,
        'rustflags': os.environ.get('RUSTFLAGS'),
        'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
        'uname': platform.uname()._asdict(), 'cpu_before': cpu_snapshot(),
        'scope': 'single-thread production configuration API lookup, System allocator, no network; Criterion function samples are sequential, not paired proxy rounds',
    }
    (output/'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    env = dict(os.environ, ZELVIK_UDP_DEMUX_BENCH_OUTPUT=str(output/'criterion'))
    start = time.monotonic()
    with (output/'benchmark.log').open('w') as log:
        result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
    metadata.update(elapsed_seconds=time.monotonic()-start, cpu_after=cpu_snapshot(), exit_code=result.returncode,
                    binaries_unchanged=metadata['binaries']=={str(binary): digest(binary)},
                    sources_unchanged=hashes=={str(path): digest(path) for path in sources})
    (output/'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    assert result.returncode == 0, f'benchmark exited {result.returncode}; see benchmark.log'
    assert metadata['binaries_unchanged'] and metadata['sources_unchanged']
    cases = {}
    for path in (output/'criterion').glob('**/new/benchmark.json'):
        identity = json.loads(path.read_text())
        sample = json.loads(path.with_name('sample.json').read_text())
        assert len(sample['iters']) == len(sample['times']) == 30
        assert all(n > 0 for n in sample['iters']+sample['times'])
        per_iteration = [elapsed/count for count, elapsed in zip(sample['iters'], sample['times'])]
        key = (identity['group_id'], identity['function_id'], identity['value_str'])
        assert key not in cases
        cases[key] = {'mean_ns': statistics.fmean(per_iteration),
                      'min_sample_ns': min(per_iteration), 'max_sample_ns': max(per_iteration),
                      'sample_count': len(per_iteration), 'iterations': sum(sample['iters']),
                      'raw_samples': str(path.with_name('sample.json').relative_to(output))}
    expected = {(f'udp_demux/sites{sites}-policy{policy}', method, str(port))
                for sites in [1, 1000] for policy in [8443, 0]
                for method in ['sets', 'point'] for port in [443, 8443, 5300, 9999]}
    assert set(cases) == expected, set(cases)^expected
    comparisons = {}
    for group, method, port in sorted(cases):
        if method == 'point':
            old, new = cases[(group, 'sets', port)], cases[(group, 'point', port)]
            comparisons[group+'/'+port] = {'sets': old, 'point': new,
                'change_pct': (new['mean_ns']/old['mean_ns']-1)*100}
    summary = {'passed': True, 'functions': len(cases), 'comparisons': comparisons,
               'interpretation': 'Negative lookup time is improvement. Means recomputed from raw samples; no proxy RPS or maximum-capacity inference. See raw Criterion estimates and outliers.'}
    (output/'summary.json').write_text(json.dumps(summary, indent=2)+'\n')
    print(json.dumps({'passed': True, 'functions': len(cases), 'output': str(output)}))


if __name__ == '__main__':
    main()
