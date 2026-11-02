"""Finish the report using immutable case summaries and corrected CPU groups."""
import json
from pathlib import Path

base=Path('docs/evidence/h3-2026-11-02')
report=Path('docs/performance-h3-2026-11-02.md')
text=report.read_text()
tables=Path('.local/h3-stage1/results-tables.md').read_text()
profiles=json.loads((base/'profiles-after-summary.json').read_text())
by_state={p['state']:p for p in profiles['profiles']}
lines=['## 用户态 CPU 采样与剩余差距','',
    'H3 HIT/OFF 各采集 15 秒、199 Hz，共 4,585 个用户态样本，记录的丢失样本为 0。只附着 Edge PID，排除内核和控制端。采样、报告处理会改变时序，其 RPS/CPU 不进入容量表。包含栈分组互相重叠，不能相加或当作删除功能后的收益。', '',
    '协议分组匹配实际函数栈所属 crate，避免把 Pingora 泛型参数中的 `origin_h3::OriginH3Connector` 误算为 h3 协议调用。原始宽泛分类摘要保留在本地并记录摘要，修正算法随证据归档。内联与不可见符号仍会限制归因。','',
    '| 可见包含栈组 | HIT | OFF |','| --- | ---: | ---: |']
for key,label in [('quic_demux','共享 UDP 分流'),('memory_governor','资源治理'),
    ('governor_snapshot_in_demux','其中：分流内治理快照'),('h3_adapter','自有 H3 适配'),
    ('h3_protocol','h3 协议实现'),('quinn_protocol','quinn-proto 协议处理'),
    ('h3_quinn_adapter','h3-quinn 适配'),('headers','头处理'),('allocator','分配器'),
    ('cache_admission','缓存准入'),('dimension_worker','HTTP 维度统计 worker')]:
    values=[f'{by_state[state]["groups"][key]["percent"]:.2f}%' for state in ['hit','off']]
    lines.append('| '+label+' | '+' | '.join(values)+' |')
lines+=['',
    '1 KiB、reuse HIT 的观测中位数为：Edge **5,122 RPS / 274.71 µs**，Nginx **5,302 / 144.52**，OpenResty **5,170 / 139.74**。Edge 的 CPU/响应约为两者的 1.90/1.97 倍。这三组均触发客户端 CPU 超过两核预算 80% 的标记，最大单核 steal 中位数约 47–50%；RPS 接近不能证明服务端峰值或效率相同。', '',
    '后续优先级由当前代码与采样支持，但尚未实施：', '',
    '| 顺序 | 工作 | 必须保留和验证的条件 |',
    '| --- | --- | --- |',
    '| 1 | 优先处理已有路由，避免每包在查路由前构造完整治理快照；拆出轻量预算读取 | 临界压力、队列硬边界、封禁和配置代切换不能失效；对正常与压力流量分别测 CPU/响应 |',
    '| 2 | 优化共享 UDP 批收发、复制和队列；评估纯 H3 专用原生 socket | 保持混合端口、CID/迁移、动态切换与 AF_XDP 契约，先建立原生 I/O 的对照 |',
    '| 3 | 对 OFF 的无匹配缓存规则路径做消融，减少无需消费的上下文准备 | 不跳过缓存条件、正文证明、身份隔离或实际启用的功能，复测 OFF 回退 |',
    '| 4 | 建立 tokio-quiche 的同功能替换后端再比较 | 同一证书、密码组、MTU、连接模式及 CDN 路径，逐项回归缓存/取消/背压；不预先宣称裸 quiche 或去掉 Tokio 更快 |',
    '',
    '没有旧版 H3 CPU 采样和单项消融，因此本表不能确定每项修改对回退的贡献；它只描述当前候选的可见 CPU 工作。', '']
validation=json.loads((base/'measurement-validation.json').read_text())
assert validation['capacity_cases']==105 and validation['candidate_statuses']=={'passed':33}
intro='''## 独立对照与验证

105 个容量场景中，86 通过、17 失败、2 个包含传输错误；**新 Edge 的 33 个容量场景全部通过、客户端零失败**。另有 5 个候选功能场景全部通过，不计入容量数量：128 KiB MISS 校验 128 个响应，HIT 校验 2,471 个响应，均零失败；加上三个冒烟场景共 26,823 个成功响应。

17 个没有完整原生结果的场景包括旧版 16 个缓存 BYPASS 状态失败，以及 Nginx 一次 MISS multiplex 批次超 90 秒。两个完整但含传输错误的场景为旧 Edge fresh OFF 的 121 次失败、OpenResty OFF multiplex 的 16 次失败；已知失败传输合计 137，未包含超时批次未知的完整错误数。原始数据全部保留，尚未把竞品错误归因为单一客户端、服务端或网络缺陷。七组旧版/额外诊断运行及两次 CPU 采样也不计入容量场景数。

'''
replacement=intro+tables+'\n'+'\n'.join(lines)
assert '<!-- TEST_AND_MEASUREMENT_RESULTS -->' in text
text=text.replace('<!-- TEST_AND_MEASUREMENT_RESULTS -->',replacement)
assert '<!--' not in text
report.write_text(text)
print('H3 report completed')
