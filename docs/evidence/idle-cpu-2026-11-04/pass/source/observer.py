#!/usr/bin/env python3
"""Read host cgroup/process counters; never execute an observer inside the proxy."""
import os
from pathlib import Path, PurePosixPath
import time


def stat(raw):
    left, right = raw.index('('), raw.rindex(')')
    values = raw[right + 2:].split()
    return dict(pid=int(raw[:left].strip()), comm=raw[left + 1:right],
                state=values[0], user_ticks=int(values[11]),
                system_ticks=int(values[12]), start_ticks=int(values[19]))


def process(pid):
    path = Path(f'/proc/{pid}')
    before = stat((path / 'stat').read_text())
    threads, exited = [], []
    for entry in sorted((path / 'task').iterdir(), key=lambda row: int(row.name)):
        try:
            threads.append(stat((entry / 'stat').read_text()))
        except FileNotFoundError:
            current = stat((path / 'stat').read_text())
            if current['start_ticks'] != before['start_ticks'] or entry.exists():
                raise
            exited.append(int(entry.name))
    after = stat((path / 'stat').read_text())
    assert before['start_ticks'] == after['start_ticks'], 'process identity changed during snapshot'
    return dict(process=after, threads=threads, exited_during_snapshot=exited)


def proxy_sample(current_target, stem):
    if Path(stem).name == 'origin-reference':
        return False
    if current_target is None:
        raise RuntimeError('proxy workload has no observed target')
    return True


def cgroup_path(pid):
    rows = Path(f'/proc/{pid}/cgroup').read_text().splitlines()
    assert len(rows) == 1 and rows[0].startswith('0::'), 'unified host cgroup namespace required'
    relative = PurePosixPath(rows[0][3:])
    assert relative.is_absolute() and '..' not in relative.parts
    root = Path('/host-cgroup')
    path = root / str(relative).lstrip('/')
    assert path.resolve().is_relative_to(root.resolve())
    assert path != root, 'proxy must have an independent child cgroup'
    return path


def counter_file(path):
    return {key: int(value) for key, value in (line.split() for line in path.read_text().splitlines())}


def snapshot(folder, pids, host_cpu):
    start = time.monotonic_ns()
    processes = [process(pid) for pid in pids]
    data = dict(monotonic_ns=start, processes=processes,
        cpu=counter_file(folder / 'cpu.stat'),
        memory_current=int((folder / 'memory.current').read_text()),
        memory_peak=int((folder / 'memory.peak').read_text()),
        memory_events=counter_file(folder / 'memory.events'),
        cpu_max=(folder / 'cpu.max').read_text().strip(),
        memory_max=(folder / 'memory.max').read_text().strip(),
        host_cpu=host_cpu())
    data['snapshot_finished_ns'] = time.monotonic_ns()
    return data


def delta(before, after, hertz):
    seconds = (after['monotonic_ns'] - before['monotonic_ns']) / 1e9
    assert seconds > 0
    cpu = {key: after['cpu'][key] - before['cpu'][key]
           for key in ['usage_usec', 'user_usec', 'system_usec']}
    assert all(value >= 0 for value in cpu.values())
    left = {row['process']['pid']: row for row in before['processes']}
    right = {row['process']['pid']: row for row in after['processes']}
    assert left.keys() == right.keys(), 'selected process set changed'
    rows = []
    for pid, old in left.items():
        new = right[pid]
        assert old['process']['start_ticks'] == new['process']['start_ticks']
        ticks = {key: new['process'][key] - old['process'][key] for key in ['user_ticks', 'system_ticks']}
        assert all(value >= 0 for value in ticks.values())
        a = {(row['pid'], row['start_ticks']): row for row in old['threads']}
        b = {(row['pid'], row['start_ticks']): row for row in new['threads']}
        stable = []
        for key in sorted(a.keys() & b.keys()):
            user = b[key]['user_ticks'] - a[key]['user_ticks']
            system = b[key]['system_ticks'] - a[key]['system_ticks']
            assert user >= 0 and system >= 0
            stable.append(dict(tid=key[0], start_ticks=key[1], comm=b[key]['comm'],
                               user_ticks=user, system_ticks=system))
        rows.append(dict(pid=pid, comm=new['process']['comm'], **ticks,
            stable_threads=stable, new_thread_identities=sorted(b.keys() - a.keys()),
            exited_thread_identities=sorted(a.keys() - b.keys()),
            thread_cpu_is_only_stable_survivors=True))
    return dict(seconds=seconds, cpu_usec=cpu, cpu_cores=cpu['usage_usec'] / (seconds * 1e6),
                process_cpu=rows, clock_ticks_per_second=hertz,
                thread_ticks_are_not_exact_cgroup_cpu=True)


def interval(folder, pids, host_cpu, write, path, seconds=10):
    samples = [snapshot(folder, pids, host_cpu)]
    started = time.monotonic()
    for number in range(1, seconds + 1):
        time.sleep(max(0, started + number - time.monotonic()))
        samples.append(snapshot(folder, pids, host_cpu))
        write(path, dict(complete=False, snapshots=samples))
    result = dict(complete=True, requested_seconds=seconds, snapshots=samples,
                  summary=delta(samples[0], samples[-1], os.sysconf('SC_CLK_TCK')))
    write(path, result)
    return result
