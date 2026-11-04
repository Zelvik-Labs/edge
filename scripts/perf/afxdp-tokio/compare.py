#!/usr/bin/env python3
"""Compare actual AF_XDP wake variants after every compiler has exited."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import subprocess
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def cpu():
    selected = {"cpu", *(f"cpu{n}" for n in os.sched_getaffinity(0))}
    proc = {p[0]: [int(v) for v in p[1:]] for line in Path('/proc/stat').read_text().splitlines() if (p := line.split()) and p[0] in selected}
    child = resource.getrusage(resource.RUSAGE_CHILDREN)
    stat = Path('/sys/fs/cgroup/cpu.stat')
    return {"proc": proc, "cgroup_cpu_stat": stat.read_text() if stat.exists() else None, "child_user_seconds": child.ru_utime, "child_system_seconds": child.ru_stime}


def parse_cpus(text):
    cpus = set()
    for part in text.split(','):
        bounds = [int(v) for v in part.split('-')]
        cpus.update(range(bounds[0], bounds[-1] + 1))
    return cpus


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--generated-worker', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--roundtrips', type=int, default=100000)
    p.add_argument('--warmup', type=int, default=2000)
    p.add_argument('--rounds', type=int, default=3)
    args = p.parse_args()
    if not 0 < args.roundtrips <= 1000000 or not 0 <= args.warmup <= 1000000 or args.rounds < 1:
        p.error('invalid iteration count')
    root = Path(__file__).resolve().parents[3]
    binary = args.binary.resolve(strict=True)
    generated = args.generated_worker.resolve(strict=True)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    cpus = sorted(os.sched_getaffinity(0))[:2]
    if len(cpus) != 2:
        p.error('two allowed CPUs required')
    os.sched_setaffinity(0, cpus)
    legacy = root/'src/xdp/af_xdp/wake.rs'
    prefix = legacy.read_text().split('\n#[cfg(test)]\nmod tests {', 1)[0]
    expected = ''.join(('//' + line[3:] if line.startswith('//!') else line) + '\n' for line in prefix.splitlines())
    if generated.read_text() != expected:
        raise SystemExit('compiled legacy worker differs from the node runtime source')
    sources = sorted((root/'scripts/perf/afxdp-tokio').rglob('*'))
    sources = [f for f in sources if f.is_file() and f.suffix in ('.rs', '.toml', '.lock', '.c', '.py', '.sh') and '__pycache__' not in f.parts]
    sources += [legacy, root/'scripts/perf/afxdp-wake.c']
    meta = {'schema_version': 1, 'scope': 'single-process AF_PACKET client + forced-copy XSK echo; not full Edge capacity', 'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(), 'tracked_dirty': bool(subprocess.check_output(['git', 'diff', 'HEAD', '--name-only'], cwd=root, text=True).strip()), 'kernel': platform.uname()._asdict(), 'cpus': cpus, 'roundtrips': args.roundtrips, 'warmup': args.warmup, 'rounds': args.rounds, 'binary_sha256': digest(binary), 'generated_worker_sha256': digest(generated), 'legacy_runtime_matches': True, 'sources': {str(f.relative_to(root)): digest(f) for f in sources}, 'started_unix': time.time()}
    cpu_max = Path('/sys/fs/cgroup/cpu.max')
    meta['cgroup_cpu_max'] = cpu_max.read_text().strip() if cpu_max.exists() else None
    write(out/'metadata.json', meta)
    records = []
    failed = False
    def run(engine, directory, functional=False, number=None):
        directory.mkdir()
        result = directory/'result.json'
        command = [str(binary), '--rx', 'ew-rx', '--peer', 'ew-peer', '--engine', engine, '--output', str(result)]
        if functional:
            command += ['--functional']
        else:
            command += ['--warmup', str(args.warmup), '--roundtrips', str(args.roundtrips)]
        record = {'round': number, 'engine': engine, 'functional': functional, 'command': command, 'cpu_before': cpu(), 'started_unix': time.time()}
        with (directory/'stdout.log').open('w') as stdout, (directory/'stderr.log').open('w') as stderr:
            try:
                record['returncode'] = subprocess.run(command, cwd=root, stdout=stdout, stderr=stderr, timeout=180).returncode
            except subprocess.TimeoutExpired:
                record.update(returncode=None, timeout_seconds=180)
        record.update(cpu_after=cpu(), finished_unix=time.time())
        data = json.loads(result.read_text()) if result.exists() else None
        record['result'] = data
        passed = record['returncode'] == 0 and data is not None and data.get('passed') is True
        if passed and not functional:
            native = data['native']
            passed &= all(native[k] == args.warmup + args.roundtrips for k in ('received', 'submitted', 'completed'))
            threads = data['resources']['threads']
            passed &= all(parse_cpus(t['cpus']) and parse_cpus(t['cpus']) <= set(cpus) for t in threads)
            passed &= sum(t['name'] == 'xsk-wake-bench' for t in threads) == (1 if engine == 'worker' else 0)
            passed &= data['wake_waits'] > 0 and len(data['latency_samples_ns']) == args.roundtrips
            passed &= data['measured_wake_waits'] >= args.roundtrips - 1
        record['passed'] = bool(passed)
        # Samples stay in each raw result; the top-level index stays small.
        index = {**record, 'result': {k: v for k, v in data.items() if k != 'latency_samples_ns'} if data else None}
        write(directory/'validation.json', index)
        records.append(index)
        write(out/'runs.json', records)
        print(f'engine={engine} round={number} functional={functional} passed={passed}', flush=True)
        return passed
    for engine in ('readable', 'read-write'):
        failed |= not run(engine, out/f'functional-{engine}', True)
    orders = [('worker', 'readable', 'read-write'), ('read-write', 'readable', 'worker'), ('readable', 'worker', 'read-write')]
    for number in range(1, args.rounds+1):
        for engine in orders[(number-1) % len(orders)]:
            failed |= not run(engine, out/f'round-{number}-{engine}', number=number)
    comparisons = []
    for engine in ('readable', 'read-write'):
        pairs = []
        for number in range(1, args.rounds+1):
            values = {r['engine']: r['result']['mean_roundtrip_ns'] for r in records if r['round'] == number and r['passed']}
            if 'worker' in values and engine in values:
                pairs.append({'round': number, 'worker': values['worker'], engine: values[engine], 'change_percent': (values[engine]/values['worker']-1)*100})
        comparisons.append({'engine': engine, 'pairs': pairs, 'median_percent': statistics.median(x['change_percent'] for x in pairs) if pairs else None})
    meta.update(finished_unix=time.time(), binary_unchanged=digest(binary) == meta['binary_sha256'], generated_worker_unchanged=digest(generated) == meta['generated_worker_sha256'], sources_unchanged=all(digest(root/name) == h for name,h in meta['sources'].items()))
    write(out/'metadata.json', meta)
    passed = not failed and meta['binary_unchanged'] and meta['sources_unchanged'] and meta['generated_worker_unchanged']
    write(out/'summary.json', {'all_passed': passed, 'comparisons': comparisons})
    if not passed:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
