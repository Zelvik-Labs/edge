"""Render all measured groups, preserving single-round and batch boundaries."""
import json
from pathlib import Path

base = Path('docs/evidence/architecture-stage2-2026-11-02')
lines = []
specs = [
    ('h1', 'H1、1 KiB、连接复用、三轮', 'HIT/OFF 每窗口 8 秒；MISS 每批 4096 个唯一键。'),
    ('tls-h2', 'TLS H1 / H2、1 KiB、单轮', 'reuse 为 32 连接；multiplex 为 2 连接 × 16 在途。HIT/OFF 8 秒，MISS 4096 唯一键。'),
    ('fresh', 'H1 / TLS H1 / H2、每请求新连接、默认算法、单轮', '各状态均为 2048 请求有界批次，TLS session cache 关闭。默认密钥交换组不同，TLS 数据不属于密码算法等价比较；统一算法另表报告。'),
    ('uniform-tls', 'TLS H1 / H2、每请求新连接、统一算法、单轮', '每状态 2048 请求。客户端统一 TLS 1.3、TLS_AES_256_GCM_SHA384、X25519；RSA 2048 证书和 RSASSA-PSS 签名。逐场景前后探针确认协议、套件、组和证书验证；原生客户端使用同一 OpenSSL 配置。'),
    ('cold-long', 'H1 冷缓存、32768 唯一键、三轮', '较长有界批次仍不等于长期稳态；各产品均须逐请求真实回源。'),
    ('32k', 'H1、32 KiB、连接复用、单轮', 'HIT/OFF 每窗口 5 秒；MISS 512 个唯一键，只作短批次探索。'),
]
for name, title, notes in specs:
    result = json.loads((base / name / 'results.json').read_text())
    groups = {}
    for group in result['groups']:
        key = group['protocol'], group['connection'], group['cache']
        groups.setdefault(key, {})[group['label']] = group
    lines += ['### ' + title, '', notes + ' 单元格为 RPS / CPU µs/成功响应；多轮取各指标中位数。', '',
              '| 协议 / 连接 / 状态 | 上一版 Edge | 本轮 Edge | Nginx | OpenResty | Edge CPU 变化 |',
              '| --- | ---: | ---: | ---: | ---: | ---: |']
    for key, products in sorted(groups.items()):
        cells = []
        for label in ['baseline', 'candidate', 'nginx', 'openresty']:
            group = products[label]
            rps, cpu = group['good_rps_median'], group['cpu_usec_per_200_median']
            text = f'{rps:,.0f} / {cpu:.2f}' if rps is not None and cpu is not None else '未知'
            if group['passed'] < group['samples']:
                text += f'（未通过 {group["samples"] - group["passed"]}）'
            cells.append(text)
        old, new = [products[label]['cpu_usec_per_200_median'] for label in ['baseline', 'candidate']]
        change = f'{100*(new/old-1):+.1f}%' if old and new is not None else '未知'
        lines.append('| ' + ' / '.join([*key[:2], key[2].upper()]) + ' | ' + ' | '.join(cells) + ' | ' + change + ' |')
    errors = sum(c['failed'] or 0 for c in result['cases'])
    passed = sum(c['status'] == 'passed' for c in result['cases'])
    lines += ['', f'通过 {passed}/{result["case_count"]} 个场景，已知原生错误 {errors}。', '']
Path('.local/architecture-stage2/capacity-tables.md').write_text('\n'.join(lines))
