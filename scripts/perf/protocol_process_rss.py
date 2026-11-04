#!/usr/bin/env python3
"""Keep process RSS snapshots separate from cgroup memory and shared-page totals."""
import argparse
from collections import Counter, defaultdict
import csv
import json
from pathlib import Path
import statistics


def parse_processes(raw):
    lines = raw.splitlines()
    assert lines and lines[0].split() == ['PID', 'COMMAND', 'RSS', 'NLWP']
    result = []
    for line in lines[1:]:
        pid, command, rss, threads = line.split()
        row = {'pid': int(pid), 'command': command, 'rss_kib': int(rss), 'threads': int(threads)}
        assert row['pid'] > 0 and row['rss_kib'] >= 0 and row['threads'] > 0
        result.append(row)
    assert result and len({row['pid'] for row in result}) == len(result)
    return result


def collect(root):
    rows, missing = [], []
    for path in sorted(root.glob('*/manifest.json')):
        manifest = json.loads(path.read_text())
        for case in manifest['cases']:
            identity = {'manifest': str(path.relative_to(root)), 'stem': case['stem'],
                        'source': manifest['source_revision'], 'target': manifest['target'],
                        'backend': manifest['args'].get('edge_backend', 'quinn-shared'),
                        'batch': manifest['args'].get('quic_rx_batch', '1'),
                        'payload_bytes': manifest['args']['payload_size'],
                        **{key: case[key] for key in ['protocol', 'cache', 'connection', 'status']}}
            if not all('proxy_processes_' + side in case for side in ['before', 'after']):
                missing.append(identity)
                continue
            for side in ['before', 'after']:
                processes = parse_processes(case['proxy_processes_' + side])
                if manifest['target'] == 'edge':
                    assert sum(process['command'] == 'Zelvik-Edge' for process in processes) == 1
                rows.extend({**identity, 'observation': side, **process} for process in processes)
    return rows, missing


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    args = parser.parse_args()
    rows, missing = collect(args.root)
    (args.root / 'verified-process-rss.json').write_text(json.dumps({'processes': rows, 'missing_cases': missing}, indent=2) + '\n')
    if not rows:
        print(json.dumps({'process_observations': 0, 'missing_cases': len(missing)}))
        return
    with (args.root / 'verified-process-rss.csv').open('w') as file:
        writer = csv.DictWriter(file, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    groups = defaultdict(list)
    for row in rows:
        if row['command'] == 'Zelvik-Edge' and row['observation'] == 'after':
            # Keep production comparisons and RX/native experiments independent.
            mode = 'native' if row['manifest'].startswith('native-') else 'rx' if row['manifest'].startswith('rx-') else 'comparison'
            key = (mode, row['backend'], row['batch'], row['payload_bytes'], row['protocol'], row['cache'], row['connection'])
            groups[key].append(row)
    lines = ['# Edge 主进程 RSS 观测', '',
             '下表为成功场景结束时主进程 RSS 的中位数，不是峰值、PSS、分配器账本或优化前后差值。',
             '控制器及 Nginx/OpenResty 各进程的独立 RSS 均保留在 CSV；不把多进程 RSS 相加冒充去重物理内存。',
             '失败/缺失观测保留在 JSON，不构成完整单元格的内存结果。', '',
             '| 实验 | 后端 | 批次 | 正文 B | 协议 | 缓存 | 连接 | RSS MiB | 独立样本 |',
             '|---|---|---:|---:|---|---|---|---:|---:|']
    for key, items in sorted(groups.items()):
        expected = 2 if key[0] == 'comparison' else 3
        valid = len(items) == expected and all(row['status'] == 'passed' for row in items)
        value = f"{statistics.median(row['rss_kib'] for row in items) / 1024:.2f}" if valid else '失败/缺失'
        lines.append('| ' + ' | '.join(map(str, [*key, value, f'{len(items)}/{expected}'])) + ' |')
    (args.root / 'verified-process-rss.md').write_text('\n'.join(lines) + '\n')
    print(json.dumps({'process_observations': len(rows), 'missing_cases': len(missing),
                      'commands': dict(Counter(row['command'] for row in rows))}))


if __name__ == '__main__':
    main()
