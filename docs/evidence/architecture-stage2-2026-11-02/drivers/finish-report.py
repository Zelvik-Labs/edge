"""Populate the engineering report from the archived measurements."""
from collections import defaultdict
import json
from pathlib import Path
import statistics

base = Path('docs/evidence/architecture-stage2-2026-11-02')
local = Path('.local/architecture-stage2')
report = Path('docs/performance-architecture-stage2-2026-11-02.md')
text = report.read_text()
capacity = (local / 'capacity-tables.md').read_text()
capacity += '''
统一 TLS 表的旧 Edge TLS H1 HIT/MISS 窗口最大单核 steal 分别约为 64.6% / 77.7%，新 Edge 对应约为 32.5% / 41.6%；多个样本还触发客户端 CPU 超过 2 核预算 80% 的标记。因此这些单轮大幅改善不能直接归因于代码，不能据此宣称已超过竞品。质量标记、客户端/源站消耗和逐轮范围均保留在各组 results.json/cases.csv。
'''
text = text.replace('<!-- CAPACITY_TABLES -->', capacity)

data = json.loads((base / 'regression-combined/results.json').read_text())
groups = defaultdict(lambda: defaultdict(list))
for case in data['cases']:
    key = case['protocol'], case['connection'], case['cache']
    groups[key][case['label']].append(case)
lines = ['### 回退场景追加复测：原始一轮 + 追加两轮', '',
         '追加 32 个场景仅比较两个 Edge 版本，交叉反转执行顺序，未修改二进制。下表每格为三轮 CPU µs/成功响应，按轮次排列，初始异常值全部保留。默认 TLS 条件不与统一算法组混合。', '',
         '| 协议 / 连接 / 状态 | 上一版三轮 CPU | 本轮三轮 CPU | 两组中位数之比变化 | 每轮新/旧比值的中位数变化 |',
         '| --- | --- | --- | ---: | ---: |']
for key, labels in sorted(groups.items()):
    if not all(len(labels[label]) == 3 for label in ['baseline', 'candidate']):
        continue
    old, new = [[c['cpu_usec_per_200'] for c in sorted(labels[label], key=lambda c: c['round'])]
                for label in ['baseline', 'candidate']]
    ratio = 100 * (statistics.median(new) / statistics.median(old) - 1)
    paired = 100 * (statistics.median([b/a for a, b in zip(old, new)]) - 1)
    lines.append('| ' + ' / '.join([*key[:2], key[2].upper()]) + ' | ' +
                 ' / '.join(f'{x:.1f}' for x in old) + ' | ' +
                 ' / '.join(f'{x:.1f}' for x in new) + f' | {ratio:+.1f}% | {paired:+.1f}% |')
lines += ['', '两种统计在 H2 reuse OFF 上给出不同方向，说明时段波动和配对方式对结论有明显影响；配对也不等于同一时刻的独占硬件实验。H2 multiplex 的 HIT/OFF/MISS，以及 H2 fresh MISS，在两种统计下都仍变差，不能宣布回退已经消失。合并视图共 68 行，包含没有追加测试的原始 Edge 场景，属于已有数据的派生视图，不额外计入 188 个实验。']
text = text.replace('<!-- REGRESSION_TABLE -->', '\n'.join(lines))

profiles = {}
for label in ['before', 'after']:
    profiles[label] = {}
    for p in json.loads((base / ('profiles-' + label + '-summary.json')).read_text())['profiles']:
        state = Path(p['path']).name.split('-')[2]
        profiles[label][state] = p
lines = ['两代共 15,463 个用户态样本，记录的丢失样本为 0。表内为“旧 → 新”的包含栈采样占比。', '',
         '| 栈组 | HIT | OFF | MISS |', '| --- | ---: | ---: | ---: |']
for key, label in [('headers', '头处理'), ('cache_lookup', '缓存查询'),
                   ('allocator', '内存分配'), ('request_filter', '请求过滤'),
                   ('cache_admission', '缓存准入'), ('upstream_peer', '回源 peer 准备'),
                   ('dimension_worker', '维度统计 worker'), ('tokio_filesystem', 'Tokio 文件系统'),
                   ('file_fill_handler', '缓存填充 handler'), ('metadata_writer', '元数据写入')]:
    cells = []
    for state in ['hit', 'off', 'miss']:
        old, new = [profiles[label][state]['groups'][key]['percent'] for label in ['before', 'after']]
        cells.append(f'{old:.2f}% → {new:.2f}%')
    lines.append('| ' + label + ' | ' + ' | '.join(cells) + ' |')
text = text.replace('<!-- PROFILE_TABLE -->', '\n'.join(lines))

validation = json.loads((base / 'measurement-validation.json').read_text())
assert validation['comparison_case_count'] == 188
assert validation['comparison_statuses'] == {'passed': 188}
assert validation['candidate_statuses'] == {'passed': 55}
assert validation['known_native_failed_transfers'] == 0
build = '''
生产构建成功，用时 22 分 21 秒；候选二进制 SHA-256 为 `a57ba4470036cbf605893208bd9d61e77cdd35e05f5b1b7a224aea392dfa6478`，上一版为 `64d0dc7bae0e0ebcd8b52862c46ad7bb00d038f1f8cf4e22fd34a2fe4ab563ff`。源码冻结后未修改 Rust，归档时重新核对全部 173 项源码和四个二进制摘要。

**188/188 个容量对照场景通过功能契约，其中候选 55 个；原生客户端失败为 0。** 另有三个候选功能场景：128 KiB 冷缓存 128 个响应、命中 22,288 个响应，以及 H3 OFF 12,582 个响应，均校验完整正文且零失败。这些单列，不混入 188 场景；六次性能采样和三次默认 TLS 诊断也不混入。H3 此处只验证 OFF，未修复缓存或新连接问题。

测试容器与网络均已退出并移除；本轮保留 mbx 构建缓存，未执行 GC。没有提交、推送、部署或恢复 RKE 操作。
'''
text = text.replace('<!-- BUILD_AND_FUNCTIONAL -->', build.strip())
assert '<!-- ' not in text
report.write_text(text)
print('Report populated from archived measurements')
