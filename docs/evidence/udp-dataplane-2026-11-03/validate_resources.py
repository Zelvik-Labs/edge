"""Independent arithmetic over retained native counters and original smaps."""
import hashlib
import re


def validate_resources(record, metadata, path, resources):
    pids = record['server_before']['processes'].keys()
    for name in ['thread_cpu_before', 'thread_cpu_after', 'thread_cpu_deltas', 'memory_after_idle']:
        assert record[name].keys() == pids, name
    complete = []
    for pid in pids:
        first, last, result = (record[name][pid] for name in
                               ['thread_cpu_before', 'thread_cpu_after', 'thread_cpu_deltas'])
        assert first['pid'] == last['pid'] == result['pid'] == int(pid)
        assert first['clock_ticks'] == last['clock_ticks'] == result['clock_ticks'] == metadata['clock_ticks']
        identity = first['process_begin']['start_ticks']
        for snap in [first, last]:
            assert snap['process_begin']['pid'] == snap['process_end']['pid'] == int(pid)
            assert snap['process_begin']['start_ticks'] == snap['process_end']['start_ticks'] == identity
            assert snap['process_begin']['read_before_ns'] <= snap['process_begin']['read_after_ns']
            assert snap['process_end']['read_before_ns'] <= snap['process_end']['read_after_ns']
            assert snap['process_begin']['read_after_ns'] <= snap['process_end']['read_before_ns']
            for tid, raw in snap['threads'].items():
                assert raw['pid'] == int(tid)
                assert snap['process_begin']['read_after_ns'] <= raw['read_before_ns'] <= raw['read_after_ns'] <= snap['process_end']['read_before_ns']
        assert record['cpu_before']['monotonic_ns'] <= first['process_begin']['read_before_ns']
        assert first['process_end']['read_after_ns'] <= record['result']['timed_start_ns']
        assert record['result']['timed_end_ns'] <= last['process_begin']['read_before_ns']
        assert last['process_end']['read_after_ns'] <= record['cpu_after']['monotonic_ns']
        rows = {str(r['tid']): r for r in result['threads']}
        assert rows.keys() == first['threads'].keys() | last['threads'].keys()
        observed = []
        for tid, row in rows.items():
            a, b = first['threads'].get(tid), last['threads'].get(tid)
            expected = 'newly_observed' if a is None else 'no_longer_observed' if b is None else 'identity_changed' if a['start_ticks'] != b['start_ticks'] else 'matched'
            assert row['status'] == expected
            if expected == 'matched':
                user = b['user_ticks']-a['user_ticks']
                system = b['system_ticks']-a['system_ticks']
                assert user >= 0 and system >= 0
                assert (row['user_ticks'], row['system_ticks'], row['cpu_ticks']) == (user, system, user+system)
                assert row['comm_before'] == a['comm'] and row['comm_after'] == b['comm']
                assert row['elapsed_ns'] == b['read_before_ns']-a['read_before_ns'] > 0
                observed.append(user+system)
            else:
                assert row['cpu_ticks'] is None
        missing = first['missing']+last['missing']
        assert result['missing_during_snapshots'] == missing
        coverage = bool(rows) and not missing and all(r['status'] == 'matched' for r in rows.values())
        assert result['complete_identity_coverage'] == coverage
        complete.append(coverage)
        assert result['matched_thread_tick_sum'] == (sum(observed) if observed else None)
        ticks = lambda value: value['user_ticks']+value['system_ticks']
        low = ticks(last['process_begin'])-ticks(first['process_end'])
        high = ticks(last['process_end'])-ticks(first['process_begin'])
        assert 0 <= low <= high and result['process_tick_delta_bounds'] == [low, high]

        memory = record['memory_after_idle'][pid]
        assert memory['pid'] == int(pid) and memory['start_ticks'] == identity
        assert memory['read_after_ns'] > memory['read_before_ns'] >= record['idle_after']['monotonic_ns']
        raw = (path/memory['raw_file']).read_bytes()
        assert hashlib.sha256(raw).hexdigest() == memory['raw_sha256']
        fields = ['Size', 'Rss', 'Pss', 'Private_Clean', 'Private_Dirty', 'Shared_Clean', 'Shared_Dirty', 'Anonymous', 'Swap']
        mappings = []
        for line in raw.decode().splitlines():
            if re.match(r'^[0-9a-f]+-[0-9a-f]+\s', line):
                parts = line.split(None, 5)
                mappings.append({'address': parts[0], 'permissions': parts[1],
                                 'name': parts[5] if len(parts) > 5 else '', 'bytes': {}})
            elif ':' in line and line.split(':')[0] in fields:
                key, rest = line.split(':', 1)
                number, unit = rest.split()
                assert unit == 'kB' and int(number) >= 0
                assert key not in mappings[-1]['bytes']
                mappings[-1]['bytes'][key] = int(number)*1024
        assert mappings and all(set(m['bytes']) == set(fields) for m in mappings)
        assert memory['mapping_count'] == len(mappings)
        assert memory['totals_bytes'] == {f: sum(m['bytes'][f] for m in mappings) for f in fields}
        assert memory['largest_resident_mappings'] == sorted(mappings, key=lambda m: m['bytes']['Rss'], reverse=True)[:12]
    assert resources['thread_cpu_complete_identity_coverage'] == all(complete)
    assert ('thread_cpu_identity_coverage_incomplete' in resources['quality_flags']) == (not all(complete))
