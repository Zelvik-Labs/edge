"""Read native /proc CPU counters; missing observations never become zero work."""
import os
from pathlib import Path
import time


def parse_stat(text):
    pid, rest = text.split(' (', 1)
    comm, tail = rest.rsplit(')', 1)
    fields = tail.split()
    return {'pid': int(pid), 'comm': comm, 'user_ticks': int(fields[11]),
            'system_ticks': int(fields[12]), 'start_ticks': int(fields[19])}


def _read_stat(path):
    before = time.monotonic_ns()
    value = parse_stat(path.read_text())
    return dict(value, read_before_ns=before, read_after_ns=time.monotonic_ns())


def snapshot(pid):
    root = Path('/proc')/str(pid)
    result = {'pid': pid, 'clock_ticks': os.sysconf('SC_CLK_TCK'),
              'process_begin': _read_stat(root/'stat'), 'threads': {}, 'missing': []}
    for task in sorted((root/'task').iterdir(), key=lambda p: int(p.name)):
        try:
            stat = _read_stat(task/'stat')
        except FileNotFoundError:
            result['missing'].append({'tid': int(task.name), 'reason': 'thread disappeared during snapshot'})
            continue
        result['threads'][task.name] = stat
    result['process_end'] = _read_stat(root/'stat')
    assert result['process_begin']['start_ticks'] == result['process_end']['start_ticks'], 'process identity changed'
    return result


def _ticks(value):
    return value['user_ticks']+value['system_ticks']


def compare(before, after):
    assert before['pid'] == after['pid'] and before['clock_ticks'] == after['clock_ticks']
    assert before['process_begin']['start_ticks'] == after['process_end']['start_ticks'], 'process identity changed'
    assert before['process_end']['read_after_ns'] < after['process_begin']['read_before_ns']
    result = {'pid': before['pid'], 'clock_ticks': before['clock_ticks'], 'threads': [],
              'missing_during_snapshots': before['missing']+after['missing']}
    for tid in sorted(before['threads'].keys() | after['threads'].keys(), key=int):
        a, b = before['threads'].get(tid), after['threads'].get(tid)
        if a is None:
            row = {'tid': int(tid), 'status': 'newly_observed', 'cpu_ticks': None, 'after': b}
        elif b is None:
            row = {'tid': int(tid), 'status': 'no_longer_observed', 'cpu_ticks': None, 'before': a}
        elif a['start_ticks'] != b['start_ticks']:
            row = {'tid': int(tid), 'status': 'identity_changed', 'cpu_ticks': None, 'before': a, 'after': b}
        else:
            user, system = b['user_ticks']-a['user_ticks'], b['system_ticks']-a['system_ticks']
            assert user >= 0 and system >= 0, 'CPU counters decreased'
            row = {'tid': int(tid), 'status': 'matched', 'cpu_ticks': user+system,
                   'user_ticks': user, 'system_ticks': system, 'comm_before': a['comm'], 'comm_after': b['comm'],
                   'elapsed_ns': b['read_before_ns']-a['read_before_ns']}
        result['threads'].append(row)
    lo = _ticks(after['process_begin'])-_ticks(before['process_end'])
    hi = _ticks(after['process_end'])-_ticks(before['process_begin'])
    assert 0 <= lo <= hi
    result['process_tick_delta_bounds'] = [lo, hi]
    observed = [row['cpu_ticks'] for row in result['threads'] if row['cpu_ticks'] is not None]
    result['matched_thread_tick_sum'] = sum(observed) if observed else None
    result['complete_identity_coverage'] = not result['missing_during_snapshots'] and all(
        row['status'] == 'matched' for row in result['threads'])
    result['scope'] = 'per-thread native CPU counters over sequential read windows; no wall-time function attribution, no zero substitution for unobserved threads'
    return result
