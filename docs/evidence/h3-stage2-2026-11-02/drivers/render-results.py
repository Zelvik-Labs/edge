"""Render measured H3 scenarios without turning rejected states into zero RPS."""
from collections import defaultdict
import json
from pathlib import Path

base=Path('docs/evidence/h3-stage2-2026-11-02')
lines=[]
for name,title,note in [
    ('compare-h3','H3、1 KiB、reuse / multiplex、三轮','reuse：32 连接各 1 在途；multiplex：2 连接各 16 在途。HIT/OFF 5 秒，MISS 2048 唯一键。'),
    ('compare-fresh','H3、1 KiB、每请求新连接、单轮','每产品、每状态 512 请求。TLS session cache 关闭，成功响应必须逐个新建连接。'),
    ('compare-32k','H3、32 KiB、reuse、单轮','HIT/OFF 5 秒，MISS 512 唯一键。单轮短批次不表示长期容量。')]:
    data=json.loads((base/name/'results.json').read_text())
    groups=defaultdict(dict)
    for g in data['groups']:groups[(g['connection'],g['cache'])][g['label']]=g
    lines+=['### '+title,'',note+' 单元格为 RPS / CPU µs/成功响应，多轮分别取中位数。', '',
        '| 连接 / 状态 | H3 第一轮 | 本轮 Edge | Nginx | OpenResty |',
        '| --- | ---: | ---: | ---: | ---: |']
    for key,labels in sorted(groups.items()):
        cells=[]
        for label in ['baseline','candidate','nginx','openresty']:
            g=labels[label];rps,cpu=g['good_rps_median'],g['cpu_usec_per_200_median']
            if g['passed']!=g['samples']:
                cell=f'未通过 {g["samples"]-g["passed"]}/{g["samples"]}'
                if rps is not None:cell+=f'（观察到 {rps:,.0f} RPS）'
            else:cell=f'{rps:,.0f} / {cpu:.2f}'
            cells.append(cell)
        lines.append('| '+key[0]+' / '+key[1].upper()+' | '+' | '.join(cells)+' |')
    counts=defaultdict(int)
    for c in data['cases']:counts[c['status']]+=1
    lines+=['',f'逐场景状态：`{dict(counts)}`。未通过组不进入容量排名；完整错误、P99、内存、回源与连接数见 CSV。','']

for name,title in [('candidate-fresh-long','本轮 Edge：2048 次新连接批次'),('candidate-h1-h2','本轮 Edge：H1/H2 回归')]:
    data=json.loads((base/name/'results.json').read_text())
    lines+=['### '+title,'','| 协议 / 连接 / 状态 | RPS | CPU µs/响应 | P99 ms | 成功 / 失败 | 新连接数 |',
        '| --- | ---: | ---: | ---: | ---: | ---: |']
    for c in data['cases']:
        value=lambda key: f'{c[key]:.2f}' if c[key] is not None else '未知'
        lines.append('| '+ ' / '.join([c['protocol'],c['connection'],c['cache'].upper()])+
            ' | '+value('good_rps')+' | '+value('cpu_usec_per_200')+' | '+value('p99_ms')+
            f' | {c["successful"]} / {c["failed"]} | {c["new_connections"]} |')
    lines.append('')

v=json.loads((base/'measurement-validation.json').read_text())
lines+=['### 测量核对','',
    f'容量对照共 {v["capacity_cases"]} 场景：`{v["capacity_statuses"]}`；其中本轮 Edge {v["candidate_cases"]} 场景：`{v["candidate_statuses"]}`，本轮 Edge 原生传输失败 {v["candidate_native_failures"]}。',
    f'另列功能场景：`{v["functional"]}`。旧版、竞品错误和没有原生结果的场景均保留，不将未知错误率写为零。',
    '', '所有结果来自共享云，质量标记在 results.json；高 steal、客户端负载偏高和短批次均不剔除。默认 TLS 参数未强制统一，新连接跨产品对比同时包含密码实现与协商差异。', '']
Path('.local/h3-stage2/results-tables.md').write_text('\n'.join(lines))

# Follow-up stays separate: the initial noisy rounds are still shown above.
data=json.loads((base/'recheck-reuse/results.json').read_text())
groups={(g['cache'],g['label']):g for g in data['groups']}
extra=['','### 连接复用交错复测：10 秒、三组旧/新配对','',
    '原三轮复用结果出现明显回退与高 steal，因此追加逐场景交替的 HIT/OFF 复测；每场独立重启，单场 10 秒。全部初始结果保留。单元格分别取观测中位数，不将这组较好或较差的样本替换初始三轮。','',
    '| 状态 | 第一轮 CPU µs/响应 | 本轮 CPU µs/响应 | CPU 变化 | 第一轮 RPS | 本轮 RPS | 第一轮 P99 ms | 本轮 P99 ms |',
    '| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |']
for state in ['hit','off']:
    old=groups[state,'baseline'];new=groups[state,'candidate']
    if old['passed']==old['samples'] and new['passed']==new['samples']:
        a=old['cpu_usec_per_200_median'];b=new['cpu_usec_per_200_median']
        extra.append(f'| {state.upper()} | {a:.2f} | {b:.2f} | {(b/a-1)*100:+.1f}% | {old["good_rps_median"]:,.0f} | {new["good_rps_median"]:,.0f} | {old["p99_ms_median"]:.2f} | {new["p99_ms_median"]:.2f} |')
    else:extra.append(f'| {state.upper()} | 未全部通过 | 未全部通过 | 不排名 | — | — | — | — |')
extra+=['','原始配对顺序、steal、客户端负载、完整失败与每轮指标见 recheck-reuse 的 manifest / CSV。','']
with Path('.local/h3-stage2/results-tables.md').open('a') as out:out.write('\n'.join(extra))
