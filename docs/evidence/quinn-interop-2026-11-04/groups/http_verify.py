#!/usr/bin/env python3
"""Recompute independent CDN comparisons from retained per-transfer evidence.

Input is an extracted HTTP/native workflow artifact. A failed cell stays failed;
its partial throughput never enters a complete-cell comparison.
"""
import argparse
from collections import Counter, defaultdict
import csv
import gzip
import hashlib
import json
import math
from pathlib import Path
import statistics

VERSIONS = {'h1': '1.1', 'h1-tls': '1.1', 'h2': '2', 'h3': '3'}


def same(actual, expected):
    assert math.isclose(actual, expected, rel_tol=1e-10, abs_tol=1e-9), (actual, expected)


def validate(path, root, *, allow_incomplete=False):
    manifest = json.loads(path.read_text())
    assert manifest['status'] == 'completed' or (
        allow_incomplete and manifest['status'] == 'failed' and manifest.get('error')), path
    assert not manifest.get('cleanup_errors'), path
    roles = manifest['cpu_assignment']
    cpus = [cpu for ids in roles.values() for cpu in ids]
    assert len(set(cpus)) == len(cpus) and len(roles['client']) == 2 and len(roles['origin']) == 1
    args = manifest['args']
    size = args['payload_size']
    assert hashlib.sha256((path.parent/'payload').read_bytes()).hexdigest() == manifest['payload_sha256']
    rows = []
    for case in manifest['cases']:
        row = {key: case.get(key) for key in ['protocol', 'cache', 'connection', 'status', 'error', 'origin_requests']}
        row.update(payload_bytes=size, target=manifest['target'], source=manifest['source_revision'],
                   manifest=str(path.relative_to(root)), stem=case['stem'],
                   backend=args.get('edge_backend', 'quinn-shared'), batch=args.get('quic_rx_batch', '1'))
        rows.append(row)
        if case['status'] == 'not-supported':
            assert manifest['target'] != 'edge' and case['protocol'] == 'h3'
            assert '--with-http_v3_module' not in manifest['protocol_capability']
            continue
        assert case['fresh_instance']
        native = case.get('native')
        if native is None:
            assert case['status'] != 'passed' and case.get('error')
            continue
        counts, failures, latency, connects = 0, [], [], 0
        connection_ids = []
        for shard in range(2):
            raw = path.parent/(case['stem']+f'.{shard}.client.tsv.gz')
            shard_count = 0
            ids = set()
            with gzip.open(raw, 'rt') as source:
                for line in source:
                    code, version, new, received, seconds, first, exitcode, cid = line.strip().split('\t')
                    code, new, received, exitcode, cid = map(int, [code, new, received, exitcode, cid])
                    seconds = float(seconds)
                    valid = code == 200 and version == VERSIONS[case['protocol']] and received == size and exitcode == 0
                    if case['connection'] == 'fresh':
                        valid = valid and new == 1
                    if not valid:
                        failures.append(f'curl_{exitcode}/http_{code}/v_{version}')
                    if code == 200 and exitcode == 0:
                        latency.append(seconds)
                    counts += 1
                    shard_count += 1
                    connects += new
                    if cid >= 0:
                        ids.add(cid)
            if native['request_limit'] is not None:
                assert shard_count == native['request_limit']//2
            connection_ids.append(len(ids))
        assert counts > 0 and native['successful'] == counts-len(failures)
        assert native['failed'] == len(failures)
        assert native['error_distribution'] == dict(Counter(failures))
        assert native['new_connections'] == connects
        same(native['good_rps'], (counts-len(failures))/case['elapsed'])
        latency.sort()
        if latency:
            same(native['p99_ms'], latency[min(len(latency)-1, int(len(latency)*.99))]*1000)
        else:
            assert native['p99_ms'] is None
        if native['successful']:
            same(case['proxy_cpu_usec_per_200'], (case['proxy_after']['cpu.stat']['usage_usec']-case['proxy_before']['cpu.stat']['usage_usec'])/native['successful'])
        if 'origin_counter_before' in case:
            assert case['origin_requests'] == case['origin_counter_after']-case['origin_counter_before']-1
        steals = []
        for cpu in cpus:
            delta = [b-a for a, b in zip(case['host_cpu_before'][f'cpu{cpu}'], case['host_cpu_after'][f'cpu{cpu}'])]
            assert all(value >= 0 for value in delta[:8]) and sum(delta[:8]) > 0
            steals.append(delta[7]/sum(delta[:8]))
        seconds = case['elapsed']
        row.update(rps=native['good_rps'], p99_ms=native['p99_ms'], successful=native['successful'],
                   failed=native['failed'], elapsed=seconds, new_connections=connects,
                   unique_connections_per_client=connection_ids,
                   cpu_usec_per_request=case.get('proxy_cpu_usec_per_200'),
                   client_cpu_fraction=case['client_cpu_seconds']/(len(roles['client'])*seconds),
                   proxy_cpu_fraction=(case['proxy_after']['cpu.stat']['usage_usec']-case['proxy_before']['cpu.stat']['usage_usec'])/(len(roles['proxy'])*seconds*1e6),
                   origin_cpu_fraction=(case['origin_after']['cpu.stat']['usage_usec']-case['origin_before']['cpu.stat']['usage_usec'])/(seconds*1e6),
                   max_host_steal_fraction=max(steals),
                   cgroup_peak_mib=case['proxy_after']['memory.peak']['bytes']/2**20,
                   proxy_processes_after=case.get('proxy_processes_after'))
        if case['status'] == 'passed':
            assert not failures
            assert case['origin_requests'] == (0 if case['cache'] == 'hit' else native['successful'])
            for side in ['integrity_before', 'integrity_after']:
                assert case[side]['tls_verified'] and case[side]['protocol'] == VERSIONS[case['protocol']]
                assert case[side]['sha256'] == manifest['payload_sha256']
            assert case['proxy_after']['memory.events']['oom_kill'] == case['proxy_before']['memory.events']['oom_kill']
    return rows


def report(root, rows, mode):
    select = [row for row in rows if (row['manifest'].startswith('native-') if mode == 'native' else row['manifest'].startswith('rx-') if mode == 'rx' else row['manifest'].startswith('round-'))]
    if not select:
        return
    dimension = {'comparison': 'target', 'native': 'backend', 'rx': 'batch'}[mode]
    targets = {'comparison': ['edge', 'nginx', 'openresty'], 'native': ['quinn-shared', 'quinn-native', 'tokio-quiche-native'], 'rx': ['1', '32']}[mode]
    expected = 2 if mode == 'comparison' else 3
    groups = defaultdict(lambda: defaultdict(list))
    for row in select:
        groups[row['payload_bytes'], row['protocol'], row['cache'], row['connection']][row[dimension]].append(row)
    assert len(groups) == {'comparison': 90, 'native': 27, 'rx': 9}[mode], 'incomplete cell grid'
    lines = [f'# {mode}: 已核验协议/缓存/连接对比', '',
             'RPS、P99、建连数与 CPU/request 已从逐传输记录及计数器重算。全部重复通过才展示可比较的中位数；失败和缺失样本仍在 JSON/CSV 中。',
             'P99 为逐轮 P99 的中位数。cgroup 内存峰值包含文件页、tmpfs 和测试控制器，不能当作 Edge 进程 RSS。',
             f'预期每格 {expected} 个独立样本。源提交：'+', '.join(f'`{sha}`' for sha in sorted({r['source'] for r in select}))+'.', '',
             '| 正文 B | 协议 | 缓存 | 连接 | '+' | '.join(targets)+' |',
             '|---:|---|---|---|'+'---:|'*len(targets)]
    def cell(items, field):
        if len(items) != expected:
            return f'缺失（{len(items)}/{expected}）'
        if all(item['status'] == 'not-supported' for item in items):
            return '不支持'
        if any(item['status'] != 'passed' for item in items):
            return '失败（见原始记录）'
        return f'{statistics.median(item[field] for item in items):,.2f}'
    for key, values in sorted(groups.items()):
        lines.append('| '+' | '.join(map(str, [*key, *(cell(values[t], 'rps') for t in targets)]))+' |')
    lines += ['', '| 正文/协议/缓存/连接 | 后端 | P99 ms | CPU µs/成功请求 | cgroup峰值 MiB | 客户端CPU % | 源站CPU % | 最大核steal % |', '|---|---|---:|---:|---:|---:|---:|---:|']
    for key, values in sorted(groups.items()):
        for target in targets:
            items = values[target]
            fields = [cell(items, field) for field in ['p99_ms', 'cpu_usec_per_request', 'cgroup_peak_mib']]
            for field in ['client_cpu_fraction', 'origin_cpu_fraction', 'max_host_steal_fraction']:
                result = cell(items, field)
                fields.append(f'{100*statistics.median(item[field] for item in items):.2f}' if len(items) == expected and all(item['status'] == 'passed' for item in items) else result)
            lines.append('| '+' | '.join(['/'.join(map(str, key)), target, *fields])+' |')
    lines += ['', '短时云环境试验；少于 1 秒的限量批次、客户端/源站饱和、宿主机 steal 和跨轮波动需要逐格判断。未完成 24/72 小时稳定性、受控公网 RTT/丢包或业务 Lua/WAF 规则对照。原始失败不能视为 0 QPS。']
    (root/f'verified-{mode}.md').write_text('\n'.join(lines)+'\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('--binaries', type=Path, help='separately downloaded measured-executable artifact')
    args = parser.parse_args()
    rows = []
    for path in sorted(args.root.glob('*/manifest.json')):
        rows.extend(validate(path, args.root))
        manifest = json.loads(path.read_text())
        client = args.root/'protocol-state-client'
        assert hashlib.sha256(client.read_bytes()).hexdigest() == manifest['client_sha256']
        if args.binaries and manifest.get('binaries'):
            for filename, digest in manifest['binaries'].items():
                matches = list(args.binaries.rglob(Path(filename).name))
                assert len(matches) == 1, filename
                assert hashlib.sha256(matches[0].read_bytes()).hexdigest() == digest
    assert rows, 'no measurement manifests'
    (args.root/'verified-records.json').write_text(json.dumps(rows, ensure_ascii=False, indent=2)+'\n')
    with (args.root/'verified-cases.csv').open('w') as file:
        writer = csv.DictWriter(file, fieldnames=sorted({key for row in rows for key in row}))
        writer.writeheader()
        writer.writerows(rows)
    for mode in ['comparison', 'native', 'rx']:
        report(args.root, rows, mode)
    print(json.dumps({'cases': len(rows), 'status': dict(Counter(row['status'] for row in rows)),
                      'successful_transfers': sum(row.get('successful', 0) for row in rows)}, ensure_ascii=False))


if __name__ == '__main__':
    main()
