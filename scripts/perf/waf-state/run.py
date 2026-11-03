#!/usr/bin/env python3
"""Measure actual bounded-map source in fresh processes; retain every attempt."""
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
import traceback

ROOT = Path(__file__).resolve().parents[3]


def write(path, value):
    path.write_text(json.dumps(value, indent=2)+'\n')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cpu():
    return {'monotonic_ns': time.monotonic_ns(), 'proc': {
        row.split()[0]: list(map(int, row.split()[1:]))
        for row in Path('/proc/stat').read_text().splitlines() if row.startswith('cpu')}}


def memory():
    info = {}
    for line in Path('/proc/meminfo').read_text().splitlines():
        name, value = line.split(':', 1)
        if name in ['MemTotal', 'MemAvailable']:
            amount, unit = value.split()
            assert unit == 'kB'
            info[name] = int(amount)*1024
    assert set(info) == {'MemTotal', 'MemAvailable'}
    info['cgroup_v2'] = {}
    for name in ['memory.max', 'memory.current']:
        path = Path('/sys/fs/cgroup')/name
        info['cgroup_v2'][name] = path.read_text().strip() if path.exists() else None
    ceiling, current = (info['cgroup_v2'][k] for k in ['memory.max', 'memory.current'])
    available = info['MemAvailable']
    if ceiling is not None and current is not None and ceiling != 'max':
        available = min(available, int(ceiling)-int(current))
    assert available >= 3*1024**3, 'less than 3 GiB available for the 2.24 GB peak payload case'
    return info


def compiler_guard():
    for path in Path('/proc').glob('[0-9]*/comm'):
        try:
            if path.read_text().strip() in ['rustc', 'mbx-rustc', 'cc1', 'cc1plus', 'clang', 'ld.lld']:
                state = next(row for row in path.with_name('status').read_text().splitlines() if row.startswith('State:'))
                assert state.split()[1] == 'Z', 'compiler overlaps measurement'
        except FileNotFoundError:
            continue


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary = ROOT/'scripts/perf/waf-state/target/release/edge-waf-state-probe'
    sources = [ROOT/'src/firewall/bounded_map.rs', ROOT/'src/firewall/state.rs',
               ROOT/'src/memory_governor.rs', ROOT/'.github/workflows/waf-state-probe.yml']
    sources += [p for p in (ROOT/'scripts/perf/waf-state').rglob('*')
                if p.is_file() and p.suffix in ['.rs', '.toml', '.lock', '.py', '.md'] and 'target' not in p.parts]
    hashes = {str(p.relative_to(ROOT)): digest(p) for p in sources}
    allowed = sorted(os.sched_getaffinity(0))
    selected = allowed[0]
    os.sched_setaffinity(0, {selected})
    metadata = {'source_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                'source_sha256': hashes, 'binary_sha256': digest(binary),
                'uname': platform.uname()._asdict(), 'allowed_cpus_before': allowed,
                'measurement_cpus': sorted(os.sched_getaffinity(0)),
                'page_size': os.sysconf('SC_PAGE_SIZE'), 'clock_ticks': os.sysconf('SC_CLK_TCK'),
                'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
                'rustflags': os.environ['RUSTFLAGS'], 'memory_before': memory(),
                'capacities': [65_536, 1_677_721, 8_000_000], 'rounds': 5,
                'scope': 'representative capacities, not runtime governor capacity; System allocator, not Edge mimalloc; no network or whole-node throughput claim'}
    write(output/'metadata.json', metadata)
    records = []
    try:
        for repeat in range(1, 6):
            # Reverse size order on alternating rounds; raw values are retained.
            capacities = metadata['capacities'][::1 if repeat % 2 else -1]
            for capacity in capacities:
                compiler_guard()
                mem = memory()
                before = cpu()
                usage_before = resource.getrusage(resource.RUSAGE_CHILDREN)
                result = subprocess.run([str(binary), str(capacity), str(repeat), str(metadata['page_size'])], text=True, capture_output=True)
                usage_after = resource.getrusage(resource.RUSAGE_CHILDREN)
                after = cpu()
                label = f'r{repeat}-capacity{capacity}'
                (output/(label+'.stdout.log')).write_text(result.stdout)
                (output/(label+'.stderr.log')).write_text(result.stderr)
                assert result.returncode == 0, (label, result.returncode, result.stderr)
                row = json.loads(result.stdout)
                assert row['capacity_requested'] == capacity and row['repeat'] == repeat
                assert row['tables'] == 7 and row['segments_per_table'] == 64
                assert row['physical_slots_per_table'] == max(capacity//64, 64)*64
                assert row['slot_payload_bytes_all_tables'] == row['physical_slots_per_table']*row['slot_size_bytes']*7
                assert row['page_size'] == metadata['page_size']
                assert row['rss_live_empty_delta_bytes'] == row['rss_empty']-row['rss_before']
                assert row['verified_hits'] == row['verified_misses'] == row['lookup_iterations'] == 200_000
                assert row['live_entries_after_clear'] == 0 and len(row['first_write_ns']) == 7
                assert all(row[k] > 0 for k in ['construction_ns', 'empty_visit_ns', 'empty_retain_ns', 'empty_lookup_ns', 'hit_lookup_ns'])
                key = 'cpu'+str(selected)
                delta = [b-a for a, b in zip(before['proc'][key], after['proc'][key])]
                assert all(n >= 0 for n in delta[:8]) and sum(delta[:8]) > 0
                steal_pct = delta[7]/sum(delta[:8])*100
                row.update(cpu_before=before, cpu_after=after, memory_before=mem,
                           child_user_seconds=usage_after.ru_utime-usage_before.ru_utime,
                           child_system_seconds=usage_after.ru_stime-usage_before.ru_stime,
                           steal_pct=steal_pct, quality_flags=['host_cpu_steal_over_1pct'] if steal_pct > 1 else [])
                records.append(row)
                write(output/'records.json', records)
        assert digest(binary) == metadata['binary_sha256']
        assert hashes == {str(p.relative_to(ROOT)): digest(p) for p in sources}
        fields = ['construction_ns', 'empty_visit_ns', 'empty_retain_ns', 'empty_lookup_ns',
                  'hit_lookup_ns', 'rss_empty', 'rss_live_empty_delta_bytes', 'rss_one_entry_per_table']
        summary = {str(cap): {field: {'all': [r[field] for r in records if r['capacity_requested'] == cap],
                                     'median': statistics.median(r[field] for r in records if r['capacity_requested'] == cap)}
                             for field in fields} for cap in metadata['capacities']}
        write(output/'summary.json', {'passed': True, 'records': len(records), 'capacities': summary,
                                     'all_attempts_retained': True, 'metadata_scope': metadata['scope']})
    except Exception:
        write(output/'failure.json', {'passed': False, 'records_completed': len(records), 'traceback': traceback.format_exc()})
        raise


if __name__ == '__main__':
    main()
