"""Describe every run, paired differences and measurement limits without filtering."""
import statistics

TARGETS = ['worker', 'readable', 'nginx', 'openresty']


def process_cpu(before, after, hz):
    assert before['processes'].keys() == after['processes'].keys(), 'process tree changed'
    ticks = sum(after['processes'][pid]['ticks'] - value['ticks']
                for pid, value in before['processes'].items())
    elapsed = (after['monotonic_ns'] - before['monotonic_ns']) / 1e9
    assert ticks >= 0 and elapsed > 0
    return ticks / hz / elapsed * 100


def diagnostics(record, metadata, direct_rps):
    result = record['result']
    duration = result['elapsed_ns'] / 1e9
    window = (record['cpu_after']['monotonic_ns'] - record['cpu_before']['monotonic_ns']) / 1e9
    assert 0 < duration <= window
    resources = {
        'duration_seconds': duration,
        'snapshot_overhead_pct': (window / duration - 1) * 100,
        'client_cpu_pct_of_one_core': (result['timed_cpu_user_seconds'] + result['timed_cpu_system_seconds']) / duration * 100,
        'origin_cpu_pct_of_one_core': process_cpu(record['origin_before'], record['origin_after'], metadata['clock_ticks']),
        'rps_fraction_of_direct': result['requests_per_second'] / direct_rps,
        'per_cpu': {},
    }
    for cpu in metadata['cpus']:
        key = 'cpu' + str(cpu)
        delta = [b-a for a, b in zip(record['cpu_before']['proc'][key], record['cpu_after']['proc'][key])]
        total = sum(delta[:8])  # Guest ticks are already counted in user/nice.
        assert total > 0 and all(value >= 0 for value in delta[:8])
        resources['per_cpu'][key] = {'steal_pct': delta[7] / total * 100,
            'busy_pct': (total-delta[3]-delta[4]-delta[7]) / total * 100}
    if record['target'] != 'direct':
        resources['proxy_cpu_pct_of_one_core'] = process_cpu(record['server_before'], record['server_after'], metadata['clock_ticks'])
        resources['idle_proxy_cpu_pct_of_one_core'] = process_cpu(record['idle_before'], record['idle_after'], metadata['clock_ticks'])
        resources['proxy_fd_count'] = sum(p['fd_count'] for p in record['server_after']['processes'].values())
        resources['proxy_threads'] = sum(len(p['threads']) for p in record['server_after']['processes'].values())
    flags = []
    if duration < .5:
        flags.append('short_sample_under_0.5s')
    if resources['snapshot_overhead_pct'] > 5:
        flags.append('cpu_snapshot_window_overhead_over_5pct')
    if max(p['steal_pct'] for p in resources['per_cpu'].values()) > 1:
        flags.append('host_cpu_steal_over_1pct')
    for name in ['client', 'origin']:
        if resources[name+'_cpu_pct_of_one_core'] >= 90:
            flags.append(name+'_cpu_at_least_90pct')
    if record['target'] != 'direct' and resources['rps_fraction_of_direct'] >= .8:
        flags.append('throughput_near_direct_calibration_ceiling')
    resources['quality_flags'] = flags
    return resources


def distribution(values):
    return {'median': statistics.median(values), 'min': min(values), 'max': max(values), 'all': values}


def summarize(records, metadata):
    rounds = metadata['rounds']
    comparisons, resources = {}, {}
    for concurrency in [1, 16]:
        for payload in [64, 1200]:
            key = f'c{concurrency}-b{payload}'
            selected = [r for r in records if r['concurrency'] == concurrency and r['payload'] == payload]
            direct = [r for r in selected if r['target'] == 'direct']
            groups = {target: sorted([r for r in selected if r['target'] == target], key=lambda r: r['round']) for target in TARGETS}
            complete = (len(direct) == 1 and len(selected) == 1+4*rounds and
                        all([r['round'] for r in group] == list(range(1, rounds+1)) for group in groups.values()))
            case = {'passed': complete and all(r['passed'] for r in selected), 'complete': complete}
            if len(direct) == 1 and direct[0]['passed']:
                calibration = direct[0]['result']['requests_per_second']
                for record in selected:
                    if record['passed']:
                        label = f"{key}-{record['target']}-r{record.get('round', 0)}"
                        resources[label] = diagnostics(record, metadata, calibration)
            if case['passed']:
                case['rps'] = {t: distribution([r['result']['requests_per_second'] for r in group]) for t, group in groups.items()}
                case['p99_rtt_ns'] = {t: distribution([r['result']['p99_rtt_ns'] for r in group]) for t, group in groups.items()}
                case['paired_changes_pct'] = {}
                for candidate, baseline in [('readable', 'worker'), ('worker', 'nginx'), ('readable', 'nginx'), ('worker', 'openresty'), ('readable', 'openresty')]:
                    case['paired_changes_pct'][candidate+'_vs_'+baseline] = {
                        metric: distribution([(new['result'][metric]/old['result'][metric]-1)*100
                            for new, old in zip(groups[candidate], groups[baseline])])
                        for metric in ['requests_per_second', 'p99_rtt_ns']}
            comparisons[key] = case
    return {'passed': all(c['passed'] for c in comparisons.values()), 'comparisons': comparisons,
            'resources': resources,
            'failed_cases': [{'target': r['target'], 'concurrency': r['concurrency'], 'payload': r['payload'],
                              'round': r.get('round'), 'errors': r['errors']} for r in records if not r['passed']],
            'interpretation': 'All attempts retained. Paired changes use candidate/baseline-1, so positive RPS and negative P99 are improvements. Four balanced rounds describe dispersion, not a confidence interval. CPU windows include snapshot overhead; proxy CPU is summed across its process tree. Direct calibration is a single preceding observation, not a simultaneous capacity measurement. Quality flags do not remove samples.'}
