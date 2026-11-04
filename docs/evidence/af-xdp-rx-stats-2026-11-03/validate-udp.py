"""Independent count, timing, pairing and configuration checks for both budgets."""
import json
import math
from pathlib import Path
import statistics
import sys
import traceback

root, output = map(Path, sys.argv[1:3])
source = sys.argv[3]
targets = ['full-stats', 'bridge-stats', 'nginx', 'openresty']
checked, failed, metadata_by_cpu = [], [], {}

def affinity(snapshot, allowed):
    assert snapshot['processes']
    for process in snapshot['processes'].values():
        assert process['threads']
        for thread in process['threads']:
            actual = set()
            for part in thread['cpus'].split(','):
                bounds = list(map(int, part.split('-')))
                actual.update(range(bounds[0], bounds[-1]+1))
            assert actual and actual <= set(allowed), thread

def process_cpu(before, after, hz):
    assert before['processes'].keys() == after['processes'].keys()
    delta = sum(after['processes'][pid]['ticks']-process['ticks']
                for pid, process in before['processes'].items())
    assert delta >= 0 and after['monotonic_ns'] > before['monotonic_ns']
    return delta/hz/((after['monotonic_ns']-before['monotonic_ns'])/1e9)*100

for budget in [1, 2]:
    directory = root/f'results-cpu{budget}'
    cleanup = json.loads((directory/'cleanup.json').read_text())
    assert cleanup['cleanup_passed'] and cleanup['exit_code'] == 0
    case_root = directory/'comparison'
    metadata = json.loads((case_root/'metadata.json').read_text())
    metadata_by_cpu[budget] = metadata
    assert metadata['source_commit'] == source and metadata['comparison'] == 'rx-stats'
    assert metadata['targets'] == targets and len(metadata['proxy_cpus']) == budget
    assert len(metadata['cpus']) == 4 and len(set(metadata['cpus'])) == 4
    assert metadata['proxy_cpus'] == metadata['cpus'][:budget]
    assert metadata['rounds'] == 4 and metadata['single_flow_iterations'] == 40000
    assert metadata['iterations'] == 10000 and metadata['warmup'] == 1000
    runs = json.loads((case_root/'runs.json').read_text())
    summary = json.loads((case_root/'summary.json').read_text())
    expected = {(target, c, b, r) for target in targets for c in [1, 16] for b in [64, 1200] for r in range(1, 5)}
    expected |= {('direct', c, b, 0) for c in [1, 16] for b in [64, 1200]}
    assert len(runs) == 68
    assert {(r['target'], r['concurrency'], r['payload'], r.get('round', 0)) for r in runs} == expected
    for record in runs:
        c, b, target = record['concurrency'], record['payload'], record['target']
        label = f'direct-c{c}-b{b}' if target == 'direct' else f"r{record['round']}-c{c}-b{b}-{target}"
        try:
            assert record['passed'] and record['pin_cleanup_ok'] and not record['errors'], record['errors']
            path = case_root/label
            assert json.loads((path/'record.json').read_text()) == record
            data = json.loads((path/'client.json').read_text())
            origin = json.loads((path/'origin.json').read_text())
            assert record['result'] == data and record['origin'] == origin
            iterations = 40000 if c == 1 else 10000
            assert record['iterations_per_flow'] == data['iterations_per_flow'] == iterations
            assert len(record['proxy_cpus']) == budget
            assert record['proxy_cpus'] == metadata['proxy_cpus']
            affinity(record['origin_before'], [metadata['cpus'][3]])
            affinity(record['origin_after'], [metadata['cpus'][3]])
            affinity(record['client_after_warmup'], [metadata['cpus'][2]])
            if target != 'direct':
                for name in ['server_before', 'server_after', 'idle_before', 'idle_after']:
                    affinity(record[name], metadata['proxy_cpus'])
            for interface in ['eup-node', 'eup-peer']:
                features = (path/(interface+'-features.log')).read_text().splitlines()
                for name in ['tx-checksumming', 'tcp-segmentation-offload', 'generic-segmentation-offload', 'generic-receive-offload']:
                    assert any(line.startswith(name+': off') for line in features)
            count, sent = iterations*c, (iterations+1000)*c
            samples = json.loads((path/'samples.json').read_text())
            assert data['passed'] and origin['passed'] and len(samples) == count
            assert all(value > 0 for value in samples)
            assert data['sent'] == data['received'] == origin['received'] == origin['sent'] == sent
            assert origin['distinct_peer_ports'] == c
            assert data['elapsed_ns'] == data['timed_end_ns']-data['timed_start_ns'] > 0
            assert record['cpu_before']['monotonic_ns'] <= data['timed_start_ns'] < data['timed_end_ns'] <= record['cpu_after']['monotonic_ns']
            ordered = sorted(samples)
            assert data['p50_rtt_ns'] == ordered[(count-1)//2]
            assert data['p99_rtt_ns'] == ordered[math.ceil(count*.99)-1]
            assert abs(data['mean_rtt_ns']-statistics.fmean(samples)) < .001
            assert abs(data['requests_per_second']-count*1e9/data['elapsed_ns']) < .001
            resources = summary['resources'][f"c{c}-b{b}-{target}-r{record.get('round', 0)}"]
            duration = data['elapsed_ns']/1e9
            assert resources['duration_seconds'] == duration
            assert abs(resources['client_cpu_pct_of_one_core']-(data['timed_cpu_user_seconds']+data['timed_cpu_system_seconds'])/duration*100) < 1e-9
            assert abs(resources['origin_cpu_pct_of_one_core']-process_cpu(record['origin_before'], record['origin_after'], metadata['clock_ticks'])) < 1e-9
            if target != 'direct':
                assert abs(resources['proxy_cpu_pct_of_one_core']-process_cpu(record['server_before'], record['server_after'], metadata['clock_ticks'])) < 1e-9
            for cpu in metadata['cpus']:
                key = 'cpu'+str(cpu)
                delta = [new-old for new, old in zip(record['cpu_after']['proc'][key], record['cpu_before']['proc'][key])]
                total = sum(delta[:8])
                assert total > 0 and all(value >= 0 for value in delta[:8])
                assert resources['per_cpu'][key]['steal_pct'] == delta[7]/total*100
                assert resources['per_cpu'][key]['busy_pct'] == (total-delta[3]-delta[4]-delta[7])/total*100
            if target in ['full-stats', 'bridge-stats']:
                report = record['node_report']
                assert report['xskWakeMode'] == 'worker'
                assert report['xskRxStatsMode'] == ('full' if target == 'full-stats' else 'bridge')
                assert report['proxyReady'] and report['proxyRedirectEnabled']
                assert report['redirect'] == 2*sent and report['xskDrops'] == 0
                assert all(value == 0 for value in report['xskQueueStats'].values())
                assert report['app']['udpDatagrams'] == 0
                counters = report['udpProxyDiag']
                for name in ['received', 'upstreamTx', 'upstreamRx', 'sockIngressDelivered', 'downstreamEnq', 'txOk']:
                    assert counters[name] == sent, (name, counters[name], sent)
                assert counters['createOk'] == c
                for name in ['blocked', 'createErr', 'createNone', 'downstreamShed', 'droppedOnFail', 'noRoute', 'pendingQueueFull', 'pendingSessionsFull', 'sessionClosed', 'sessionFull', 'sockIngressShed', 'txDeferred', 'txNoRoute', 'upstreamTxErr']:
                    assert counters[name] == 0, (name, counters[name])
                for name in ['rateLimited', 'ratelimitMapFull', 'udpFwdMapFull', 'snatAllocFail']:
                    assert report[name] == 0
                config = (path/'home/configs/api_node.yaml').read_text()
                assert f"cpus: [{record['proxy_cpus'][0]}]" in config
                assert 'queues: [0]' in config and 'xskMode: copy' in config and 'attachMode: skb' in config
            elif target != 'direct':
                config = (path/'nginx.conf').read_text()
                assert f'worker_processes {budget};' in config
                assert 'proxy_requests 0;' in config and 'proxy_responses 2147483647;' in config
            checked.append({'budget': budget, 'case': label, 'rtt_samples': count, 'requests_including_warmup': sent})
        except Exception:
            failed.append({'budget': budget, 'case': label, 'error': traceback.format_exc()})
    for c in [1, 16]:
        for b in [64, 1200]:
            cell = summary['comparisons'][f'c{c}-b{b}']
            selected = [record for record in runs if record['concurrency'] == c and record['payload'] == b]
            assert cell['passed'] == all(record['passed'] for record in selected)
            if cell['passed']:
                groups = {target: sorted([r for r in selected if r['target'] == target], key=lambda r: r['round']) for target in targets}
                for target, group in groups.items():
                    assert cell['rps'][target]['all'] == [r['result']['requests_per_second'] for r in group]
                    assert cell['rps'][target]['median'] == statistics.median(r['result']['requests_per_second'] for r in group)
                    assert cell['p99_rtt_ns'][target]['all'] == [r['result']['p99_rtt_ns'] for r in group]
                    assert cell['p99_rtt_ns'][target]['median'] == statistics.median(r['result']['p99_rtt_ns'] for r in group)
                for pair, metrics in cell['paired_changes_pct'].items():
                    candidate, baseline = pair.split('_vs_')
                    for metric, distribution in metrics.items():
                        values = [(new['result'][metric]/old['result'][metric]-1)*100 for new, old in zip(groups[candidate], groups[baseline])]
                        assert distribution['all'] == values and distribution['median'] == statistics.median(values)
    assert summary['binaries_unchanged'] and summary['sources_unchanged']
assert metadata_by_cpu[1]['binaries'] == metadata_by_cpu[2]['binaries']
assert metadata_by_cpu[1]['sources'] == metadata_by_cpu[2]['sources']
result = {'passed': not failed, 'source_commit': source, 'checked_cases': len(checked),
          'rtt_samples': sum(case['rtt_samples'] for case in checked), 'failures': failed,
          'same_binaries_and_sources_across_budgets': True, 'checks': checked,
          'scope': 'independent recomputation of retained outputs; full byte/sequence verification happened in the pinned C client, not reconstructed from RTTs'}
output.write_text(json.dumps(result, indent=2)+'\n')
print(json.dumps({key: value for key, value in result.items() if key != 'checks'}))
sys.exit(0 if result['passed'] else 1)
