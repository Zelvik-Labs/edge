"""Render observed values; keep the selection decision for manual review."""
import json
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-recv-ablation';base=root/'docs/evidence/h3-recv-ablation-2026-11-02'
x=json.loads((base/'comparison/results.json').read_text());g={(i['cache'],i['label']):i for i in x['groups']};m=json.loads((base/'metadata.json').read_text());tests=json.loads((base/'test-results.json').read_text())
lines=['# H3 接收批量化消融（2026-11-02）','',
'上一轮[联合候选](performance-h3-stage2-2026-11-02.md)消除了分流内完整治理快照，但没有证明 HTTP 吞吐或 P99 全面改善。因此只撤回接收批量化，对三个版本逐项轮换测试，隔离这一项变化。','',
'<!-- SELECTION -->','',
'| 版本 | 治理压力读取 | 接收方式 |','| --- | --- | --- |','| baseline | 原完整快照 | 原单包返回 |','| batched | 轻量快照 | 填充多个可用接收缓冲区 |','| candidate | 与 batched 相同的轻量快照 | 恢复原单包返回 |','',
'candidate 与 batched 的 Rust 差异仅为 quic_udp_demux.rs：恢复原有 socket 接收实现，并随撤回实验删除两项仅针对新批量行为的测试。全部原有错误、ECN、队列记账及 NAT 接收回归保留；未放宽正文、证书、超时或资源准入。','',
'## 同条件三方对照','',
'每状态每版本三轮、单场 10 秒、32 连接各 1 在途，单个场景独立重启目标与 tmpfs。每轮轮换三版本顺序，源站仍为 H1，目标 2 CPU / 2 GiB、客户端 2 CPU、源站 1 CPU；父级总配额仍为 4 CPU。下表为 RPS / CPU µs/成功响应 / P99 ms，各项独立取中位数。','',
'| 状态 | baseline | batched | candidate |','| --- | ---: | ---: | ---: |']
for state in ['hit','off']:
    cells=[]
    for label in ['baseline','batched','candidate']:
        a=g[state,label];assert a['passed']==a['samples']==3
        cells.append(f'{a["good_rps_median"]:,.0f} / {a["cpu_usec_per_200_median"]:.2f} / {a["p99_ms_median"]:.2f}')
    lines.append('| '+state.upper()+' | '+' | '.join(cells)+' |')
lines+=['','| 状态 | candidate / batched CPU 变化 | RPS 变化 | P99 变化 |','| --- | ---: | ---: | ---: |']
for state in ['hit','off']:
    a=g[state,'batched'];b=g[state,'candidate'];values=[(b[k]/a[k]-1)*100 for k in ['cpu_usec_per_200_median','good_rps_median','p99_ms_median']]
    lines.append('| '+state.upper()+' | '+' | '.join(f'{v:+.1f}%' for v in values)+' |')
lines+=['','<!-- INTERPRETATION -->','',
'共享云的 steal、客户端 CPU 瓶颈、三轮波动和目标控制端 CPU 全部保留。这里不声称测得服务器峰值容量，也不将微基准的快照 API 比例当作 HTTP 提升比例。Nginx/OpenResty 的完整协议与状态对照保留在上一轮报告，本轮不将不同时段的竞品数值混入同一组三方排名。','',
'## 功能与溯源','',
f'18 个对照场景、7 个候选功能场景通过，原生客户端错误为零。候选功能包括 H3 MISS/HIT/OFF、128 KiB MISS/HIT，以及每状态 2048 次新连接的 HIT/OFF。相关 Rust 测试去重 {tests["passed_unique"]} 项通过、{tests["ignored_unique"]} 项原有测试忽略，Clippy 与发行构建通过。', '',
f'候选二进制 SHA-256：`{m["binaries"]["candidate"]["sha256"]}`；批量候选为 `{m["binaries"]["batched"]["sha256"]}`，基线为 `{m["binaries"]["baseline"]["sha256"]}`。源码、可执行文件及工具摘要冻结，补丁明确展示仅接收实现发生变化。', '',
'云构建缓存已按用户授权放宽，原 12 GiB 人为配额不再用于该云工作区；仍保留磁盘最低可用空间要求。编译与测量串行，测试服务的 CPU、内存与业务缓存条件未调整。','',
'原始 TSV、日志、可移植索引、全部逐轮指标及复算方法见[证据说明](evidence/h3-recv-ablation-2026-11-02/README.md)。','',
'本轮没有引入 tokio-quiche；发布版本及接入差异见[源码评估](performance-h3-tokio-quiche-evaluation-2026-11-02.md)。H3 回源池、实际 AF_XDP 硬件、持续 churn 生命周期和 24/72 小时稳定性仍未验收；没有发布或部署。','']
(root/'docs/performance-h3-receive-ablation-2026-11-02.md').write_text('\n'.join(lines))
