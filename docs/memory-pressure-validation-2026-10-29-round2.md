# 2026-10-29 内存治理第二轮：连接公平性与续传验收

状态：代码、负向对照、Linux cgroup 验收矩阵和配对性能测量均已完成；未 push、未发版、未部署生产。

## 结论

- **A（TicketBucket 公平性）通过。** 请求 workspace 同时受全局 ledger 和 owner 上限约束，最终 owner cap 固定为全局池的 1/4，取消会在小池下占满全局的 16 MiB 下限。小池容不下的大额新 transform 直接拒绝。H2 stream 使用独立全局计数 permit，不从字节 float 绕过计数；H3 每个 QUIC 连接显式持有一个 owner bucket。
- **B（协议拒绝语义）通过。** H2 新 stream 失败只发 `RST_STREAM(REFUSED_STREAM)`，accept loop 继续；H3 新 request 用公开 h3 API 同时 reset 两个方向为 `H3_REQUEST_REJECTED`。验收在 Critical 阶段观察到已接受 H2/H3 长响应继续增长，而同一连接上的新 stream 被拒绝且连接保持存活。
- **C（Critical shed）通过，但有边界。** 删除年龄阶梯；final resort 需要持续 Critical、无效 reclaim 证据、真实快照确认以及 `account_headroom < working_set`，每次最多 drain 32 L4 + 32 H3，30 秒最多一次。ballast/mixed 512 与 256 轮次在 94% 档触发一次有界 drain，OOM kill 为 0；cache 轮次没有触发 final resort，说明可回收缓存优先退让。
- **D（存活验收）在可恢复压力下通过，硬耗尽另列。** 六轮 cache/ballast/mixed × 512/256 均完成，三个协议的已接受长响应在 final resort 之前持续前进，释放压力后 fresh 连接全部恢复。恢复后另发 fast 请求每协议均 3/3 成功；这不是原始长响应的完成率。两组 cache-only 均因生产缓存预算未达到后续目标物理压力；256 MiB 的 L1 在进入 stage 后为 0、disk 约 25–30 MiB。
- **E（性能基线）本负载未见稳定回归。** 首轮候选 native 中位数吞吐变化 −0.43%～+1.45%、CPU/request −1.44%～+0.41%；最终严格 owner cap 重测为吞吐 −1.97%～+0.36%、CPU/request −0.30%～+2.14%，各协议范围均重叠。分配次数中位数变化 ≤0.40%。Python H2/H3 单轮 P99 有离群样本，详见表和限制。

## 基线与范围

- 六组主矩阵与首轮配对性能候选：`candidate-round2-opt-node`，源码 `f2e0872`（Rust/ fixture），验收 driver `672c9d9`。首轮完整下载 driver 为 `5aa64f1`，256 MiB 分段补验为 `8bf7968`，使用同一候选二进制。
- 最终源码 `3181e71`：包括 H3 permit 持有位置清理 `6520f63` 与严格 `workspace/4` owner cap。最终二进制 `candidate-round2-fair-node` 的 SHA256 为 `f95619c38e69889766dbb8d2f3f8deb8ce7b9b392089d940f74c354b1551a174`；另行复核 mixed/256 主 ramp、原始响应完整下载与五轮性能，见最终复核一节。
- 基线二进制：`candidate-node`（`2c6b2e1`，上一轮 fixture）；两者使用同一 `Cargo.lock`、Rust 1.99.1、opt-level=2/debug=0/incremental=0/codegen-units=16、`target-cpu=native`。
- 专用 Linux 测试机 `zelvik-dev-43`，全部代理/源站/控制端口使用 loopback；每轮独立 systemd cgroup：`MemoryMax=512/256 MiB`、`MemoryHigh=90%`、swap=0、CPUQuota=200%、CPU 0–1。
- 验收 fixture 使用真实 `HttpProxyManager`、`Http3ProxyManager`、`EdgeProxy`、ConfigStore/LB、WafStateManager 和生产 TinyUFO L1；cache 源通过真实 GET 填充，不把 ballast 冒充缓存。

## A. TicketBucket 公平性与 pooled H2

### 双层 workspace

- 每个已注册 L4 连接一个 `TicketBucket`；H3 每个 QUIC 连接显式创建一个 bucket，并通过 task-local 交给 request task，避免 QUIC 无稳定 peer-address key 时错误复用。
- refill 先在 owner 上原子预留 `need + 256KiB`，再向全局 workspace ledger 申请；owner 上限为
  `workspace/4`。释放时只退还超过空闲 float 的部分，bucket drop 退还剩余全部。
- 最初候选保留 `max(workspace/4,16MiB).min(workspace)`；收尾复核确认 pool=16 MiB 时 owner 可占满全局，违反 A 项，因此最终取消该 floor。`small_workspace_owner_cannot_take_transform_floor` 用 16 MiB pool 验证 owner cap=4 MiB、16 MiB transform 拒绝、独立 16 KiB 请求仍能取得 workspace，旧 floor 的负向对照失败。续传 protected reserve 由共享账户独立优先级门保护。

### H2 stream 绕过修复

- pooled `TicketBucket::spend(AdmissionClass::Http2Stream)` 不再做字节 float spend，而是调用 `governor.try_admit(Http2Stream)`，permit 随 stream task 生命周期持有；stream 结束或连接取消时释放。
- 该路径不再额外扣 workspace 字节，但全局 `estimated_h2_streams` 和 shared connection estimate 不会漏计。
- 新增测试：`single_owner_cannot_starve_independent_workspace`、`concurrent_refills_respect_owner_cap_and_refund`、`pooled_h2_streams_enforce_global_count_and_release`。
- 负向对照：撤回 owner cap 后 `single_owner_cannot_starve_independent_workspace` 失败；撤回 H2 pooled permit 后 `pooled_h2_streams_enforce_global_count_and_release` 失败（证据日志见 `docs/evidence/memory-pressure-2026-10-29-round2/`）。

## B. H2/H3 新工作拒绝与续传

### 协议行为

- H2 per-connection/global stream admission 失败：只调用 `shutdown_with_reason(REFUSED_STREAM)` 并 `continue`，不再 `break` 整个 accept loop。
- H2 churn（200 streams/30s）仍走 GOAWAY `graceful_shutdown` 并继续 poll；这是 abuse 轨道，不是普通资源不足。
- H3 admission 失败：`reject_h3_request` 先 `resolve_request()`，再对两个方向 `stop_sending(H3_REQUEST_REJECTED)` / `stop_stream`；不 drop bare resolver，避免 h3 request id 泄漏。
- **新增 H2 timer 修复**（本轮从验收中发现）：压力 idle timeout 现在在 stream task 活跃期间滑动，不再把长响应当作 idle 连接；Critical 30s max-lifetime 不再关闭 transport，而是标记连接并逐个拒绝后续 stream，已接受 stream 继续 drain。该修复的负向对照 `h2_active_stream_slides_idle_deadline` 在撤回后失败。

### 验收观察

- Critical 档的 held H2/H3 连接上发送新 stream：`success=0/8`，错误分别为 `reset:7`（H2 REFUSED_STREAM）和 `StreamReset:267`（H3_REQUEST_REJECTED），同时 `connection_alive=true`，held 长响应继续增长（ballast 512 每档约 +8 MiB）。
- 释放压力并等到 L4 5 秒降级 dwell 后，fresh H1/H2/H3 连接的新请求为 16/16（mixed 512）；最终 normal completion 为 3/3。
- 中途恢复时 held H2 连接如果已经超过 Critical 30 秒 max-lifetime，新 stream 仍会被 `REFUSED_STREAM`；这是连接寿命策略，不是连接被中断；同一 held 连接上的已接受响应仍在传输，fresh 连接正常。
- Critical 阶段 fresh H1/H2 的 `Connection reset by peer` 主要由 L4 **每 IP TCP 活跃连接上限**（Critical 下限 16，预检保留 64+ 条 loopback 连接）造成；验收状态新增 `l4Pressure` / `tcpActivePerIpLimit` 以区分它和内存准入拒绝。恢复检查点等待 L4 降级后再探测，避免把 L4 dwell 当作内存恢复失败。

## C. Critical shed 与 final resort

- 删除年龄阶梯自动降到 0ms 后无条件终止活跃连接的行为。
- High：keepalive suppression；Critical：仍先拒绝新工作、回收可回收结构，只有同时满足以下条件才允许 final resort：
  - Critical 由新鲜 governor snapshot 确认，连续至少 10s，样本至少相隔 5s；
  - 最近一次 reclaim pass 在 30s 内且无效（RSS 改善 <4 MiB 或无条目/估算字节释放）；
  - `account_headroom_bytes` 为 `Some` 且严格小于 `connection_admission_used_bytes + request_workspace_used_bytes`；
  - 上次 final resort 至少 30s 前。
- 每次 pass 最多 drain 32 条 L4 和 32 条 H3 的最老连接，跳过已在取消中的记录，消费无效 reclaim 证据；日志记录 working set/headroom。
- 验收状态记录 `finalResortPassesTotal`、`drainedConnectionsTotal`、`criticalSamples`、`reclaimExhausted`、`lastFinalResortAtMs`；`drain_reason=MemoryPressureShed`。

### 256 MiB ballast 的 OOM 注入竞态

第一次 256 MiB ballast 轮次在 94% 填充时被 cgroup OOM killer 杀死：90% 档只有 1 个新鲜 Critical 样本，10 秒 sustain 门尚未满足，而 1 MiB ballast 填充已经到达 `memory.max`。该失败说明持续压力门无法覆盖快于采样的突发物理分配，仍可能发生 OOM；不能用调整后的慢速 ramp 通过结果推导任意注入速率下都能存活。

验收 driver 现在对非 cache 的 85/90 档停留 12 秒，让 10 秒 sustain 和 5 秒样本间隔完成；ballast 256 重跑在 94% 档触发 1 次 final resort、drain 3 条连接，held 响应在 85/90 继续前进，OOM kill 0，释放后 normal completion 3/3。final-resort 门输入（`account_headroom_bytes`、`account_committed_live_bytes`、`connection_admission_used_bytes`、`request_workspace_used_bytes`）已暴露在验收状态中，便于核对。

## D. 六轮验收矩阵

每组 1 轮；下表顺序均为 H1/H2/H3。`@90` 是目标档标签，实际物理压力见 current/peak；cache-only 的 70%–98% 目标未达到，不能将其解释为近硬限制测试。

| 来源 / 限额 MiB | @90 已接受响应增量 MiB（各协议） | 同一连接新 stream H2/H3 | final resort 次数 / drain | peak MiB | memory.high 次数 | PSI full avg10 最高样本 | OOM kill |
|---|---|---|---|---|---|---|---|
| cache / 512 | 1.42 / 1.42 / 1.43 | 8/8、8/8 | 0 / 0 | 358.89 | 0 | 0% | 0 |
| cache / 256 | 1.80 / 1.80 / 1.81 | 8/8、8/8 | 0 / 0 | 198.05 | 0 | 0% | 0 |
| ballast / 512 | 7.83 / 7.83 / 7.83 | 0/8、0/8 | 1 / 3 | 484.36 | 6630 | 62.48% | 0 |
| ballast / 256 | 7.70 / 7.70 / 7.69 | 0/8、0/8 | 1 / 3 | 256.00 | 7269 | 79.15% | 0 |
| mixed / 512 | 7.69 / 7.69 / 7.69 | 0/8、0/8 | 1 / 33 | 484.14 | 6211 | 49.99% | 0 |
| mixed / 256 | 7.66 / 7.66 / 7.67 | 0/8、0/8 | 1 / 3 | 247.89 | 3855 | 39.82% | 0 |

- 六组在中途释放压力后，fresh H1/H2/H3 各 16/16 成功，原有响应仍有字节进度。cache-only @90 的 fresh 请求各 32/32；ballast/mixed @90 的 H1 0/4，H2/H3 因建连失败各 0 次发送（请求目标各 32），不能把未发送误写为 stream 拒绝。已有 H2/H3 连接上的 8 次创建均实际发送并得到协议拒绝。
- 512 MiB cache-only 真实 L1 为 31,488,000 B，调用生产 reclaim 后为 0；此处是显式 reclaim 验证。mixed/512 的 L1 从 32,537,600 B 在 85% 压力档自然回收到 0，final resort 在后续 94% 档才触发。256 MiB cache/mixed 的 L1 已先退到 0。
- 四组 final resort 的日志均为 headroom=0、Critical fresh samples=4。ballast/256、ballast/512、mixed/256 的 working set=155,648 B，mixed/512 为 1,204,224 B；实际 drain 分别为 2 L4 + 1 H3、2+1、2+1、32+1，均未超过每类 32 的上限。阶段后的账户值可能已随请求退出归零，因此触发判据以 `final-resort-triggers.log` 为准。
- 四组硬耗尽测试的原始长响应最终每协议均 0/1 正常完成，错误为 short body / connection reset / H3 connection terminated；这是 final resort 的明确代价。两组 cache-only 的原始长响应在驱动结束前仍活跃，随后由驱动关闭，不能计为完整完成。恢复后另发的 fast 请求每协议各 3/3 成功，bounded 响应另外逐项保留在 JSON 中。
- 完整记录包含每档 `memory.current/peak/events`、PSI、拒绝原因、成功请求 P99 和 accepted-response gap P99。恢复后的 fast P99 范围：H1 2.18–3.20 ms、H2 2.37–2.74 ms、H3 5.53–9.76 ms；仅统计成功请求，须同时读取拒绝/未尝试计数。

### 原始已接受响应完整下载补验

补验在升压前为每协议建立一个响应，每条 39,321,600 B（2400×16 KiB，源站每块间隔 25 ms）。复用同一个 Sample 贯穿升压、同连接新 stream 洪峰、释放压力和完整下载；完成必须 status=200、无错误、body 长度与 SHA256 都匹配。未用恢复后新发请求替代。

| 来源 / 限额 MiB | 注入方式 | 压力阶段 | 同连接新 stream H2/H3 成功数 | 原始 H1/H2/H3 完成 | SHA256 | OOM kill |
|---|---|---|---|---|---|---|
| cache / 512 | 一步填充真实缓存 | Normal，预算封顶 | 32/32、32/32 | 各 1/1 | 全匹配 | 0 |
| cache / 256 | 一步填充真实缓存 | Normal，预算封顶 | 32/32、32/32 | 各 1/1 | 全匹配 | 0 |
| ballast / 512 | 一步目标 85% | Critical | 0/32、0/32，连接存活 | 各 1/1 | 全匹配 | 0 |
| mixed / 512 | 一步目标 85% | Critical | 0/32、0/32，连接存活 | 各 1/1 | 全匹配 | 0 |
| ballast / 256 | 50/70/85% 分段 | Critical | 0/32、0/32，连接存活 | 各 1/1 | 全匹配 | 0 |
| mixed / 256 | 50/70/85% 分段 | Critical | 0/32、0/32，连接存活 | 各 1/1 | 全匹配 | 0 |

这六组每协议 6/6 完整完成，恢复检查点原始响应均仍在传输且增量大于 0，final-resort pass 均为 0；ballast/mixed 实测压力阶段 headroom 仍有约 7.5–35.2 MiB。完整耗时约 61.4–65.1 秒；ballast/mixed 的正常完成响应块间 gap P99 约 H1/H2 26.3–26.5 ms、H3 24.6–24.7 ms。32 次同连接新 stream 被拒绝时已有响应继续前进，释放后同一响应完整结束。

**另有一组失败，未计入上述六组：** ballast/256 一步目标 85% 后，延迟的运行时工作集增长使 `memory.current=261,636,096 B`（97.47%），headroom=0，13.5 秒触发 2 L4 + 1 H3 的 final resort；三个响应分别只收到约 2.64/1.48/1.14 MiB，完成 0/1，OOM kill=0。它证明一步注入不能只按目标标签称为“可恢复的 85% 压力”。失败记录与分段补验同时保存在 `accepted-completion-summary.json`。此完整下载补验最初使用 `f2e0872` 二进制，最终严格 owner-cap 二进制的复核另列。

### 最终源码复核

`3181e71` 的 `candidate-round2-fair-node` 重新跑 mixed/256：94% 档触发 1 次有界 drain（3 条，peak 253,915,136 B，memory.high 3,492 次），OOM kill 0；中途恢复后 fresh H1/H2/H3 16/16，normal 3/3；独立完整下载三条原始响应各 39,321,600 B、SHA256 全匹配、恢复检查点仍全部活跃。这验证了严格 owner cap 的最终二进制没有改变已测的压力/续传结论。

## E. 配对性能

对比基线 `2c6b2e1` 与候选 `f2e0872`，五轮交替 AB/BA，每轮全新进程。benchmark 不开启 acceptance/cache 环境开关，配置与响应 payload 相同；候选增加的验收状态字段在 benchmark 路径关闭。native 客户端和源站分别固定 CPU 2–3、4–5；代理 CPU 0–1、配额 200%。

| 协议 | baseline RPS 中位数（范围） | candidate RPS 中位数（范围） | baseline → candidate P99 ms | baseline → candidate CPU ms/request |
|---|---|---|---|---|
| H1 | 2699.65（2659.16–2707.54） | 2724.63（2672.61–2841.29） | 18.80 → 18.72 | 0.7347 → 0.7283 |
| H2 | 2314.24（2300.91–2319.01） | 2304.25（2297.50–2316.94） | 19.789 → 20.026 | 0.8441 → 0.8476 |
| H3 | 2781.82（2763.59–2824.41） | 2822.14（2795.53–2851.23） | 20.102 → 19.739 | 0.7123 → 0.7021 |

native 中位数吞吐差为 H1 +0.93%、H2 −0.43%、H3 +1.45%；CPU/request 差为 −0.87%、+0.41%、−1.44%。范围重叠，未观察到明显回归，也不声称稳定提升。native H1/H2/H3 的 baseline 总成功数为 161286/24000/167267，candidate 为 163742/24000/169605，所有记录的请求均成功；原始错误字段保留于 summary。

| 协议 | context switches / 1000 req，baseline → candidate | futex / 1000 req，baseline → candidate | 分配次数 / success，baseline → candidate（三轮中位数） |
|---|---|---|---|
| H1 | 782.74 → 777.36 | 366.00 → 362.67 | 484.254 → 483.996 |
| H2 | 916.04 → 906.88 | 583.33 → 577.50 | 505.729 → 505.436 |
| H3 | 797.50 → 788.79 | 547.60 → 535.23 | 621.712 → 622.106 |

分配差为 −0.053%、−0.058%、+0.063%；计数包含后台线程，不能将小数差归因到单个请求。计时阶段关闭计数；分配阶段每轮每协议 1000 请求，三轮均全部成功。

Python 客户端的五轮配对每轮每协议 4000 请求，全部成功：H1 RPS/P99 中位数 2687.77/9.828 ms → 2755.93/9.428 ms；H2 1295.15/13.624 ms → 1309.90/13.301 ms；H3 463.98/40.237 ms → 462.53/40.661 ms。候选 Python H3 单轮 P99 最高为 58.305 ms（基线最高 40.957 ms），H2 最高 16.587 ms（基线 14.151 ms）；这些尾部离群样本如实保留，不以中位数掩盖。native 的五轮 P99 范围重叠，故目前证据支持“该负载下没有稳定的明显热路径回归”，不能保证所有负载无尾延迟变化。

直连源站 wrk 为 20,887.66 RPS，超过代理 H1 的 2.7k RPS；独立 CPU 样本的主要开销仍是 allocator、libc 内存操作与内核，未出现 governor/ticket 主导热点。样本保存在证据目录。

虚机不支持硬件 PMU；futex 和 context-switch 只是软件争用代理，不能据此证明 cache-line contention 为零。该 fixture 是动态 GET 转发，不包含大额 WAF transform 饱和；公平性边界由确定性并发测试和负向对照验证。

最终严格 owner cap 二进制 `candidate-round2-fair-node` 与同一 baseline 重跑五轮 native/ Python：native 吞吐变化 H1 −1.97%、H2 +0.36%、H3 +0.33%，CPU/request 变化 H1 +2.14%、H2 −0.30%、H3 −0.23%；H1 的吞吐范围与 baseline 重叠（candidate 2,610.80–2,856.92，baseline 2,684.48–2,852.17），不能把 −1.97% 归因于公平性检查本身，也不能声称稳定改善。Python 五轮每协议仍全部成功；分配次数中位数变化约 +0.01%/+0.01%/+0.40%。完整数字见 `final-fair-native-summary.json` 与 `final-fair-paired-summary.json`。

## 限制与未做事项

- 256 MiB cache-only 不能维持 L1：生产 cache-read budget 与三个已接受长响应共享，preflight 只有 4.00 MiB，进入 stage 后 L1 被回收到 0；该轮仍证明可回收 cache 不会把连接预算拖垮（held 存活、H1/H2 新请求成功、无 final resort），但不能作为“L1 持续驻留”的证据。
- H2 max-lifetime 在 Critical 仍会拒绝该连接上的后续新 stream；这是 security/lifetime 语义，不是内存准入；需要新工作的客户端应建新连接。
- 严格 owner cap 的代价：charge 大于 `workspace/4` 的新 transform 在池不足时会立即拒绝，而不是借用整个 pool。按当前类估算，16 MiB ResponseTransform 需要 workspace ≥64 MiB（Normal 约 256 MiB available，High+ 约 1 GiB available）；2 MiB RequestBodyWaf 需要 available ≥8 MiB。已有在途 charge 不受影响。这是明确 fail-closed 边界，会记录 class reject 计数。
- lifetime 路径使用逐 stream REFUSED_STREAM 保留已接受工作。验收 Python h2 客户端在 GOAWAY 后会停止处理 in-flight stream，不能独立证明 GOAWAY draining 的所有客户端互操作行为；200 streams/30s churn GOAWAY 路径未在本次压力矩阵中触发。
- 主 ramp 的已接受长响应为 393,216,000 字节，该项仅验证压力期间进度；硬耗尽后最终被 drain，cache-only 在观察结束后由驱动关闭。独立完整下载补验使用 39,321,600 字节并核对 SHA256。恢复后 `normal_completion.success=3/3` 指 fast 请求，另有 1.25 MiB bounded 响应逐项记录。
- 最终严格 owner cap 源码（`3181e71`）本地全量 lib 为 1102 passed / 0 failed / 2 ignored；H3 permit 持有位置清理（`6520f63`）后，7 项 H3 定向测试通过。correctness/suspicious clippy 退出码 0，本轮新增两条 H3 warning 已修复；另有 `origin_state.rs`、`rpc/client.rs`、`rpc/node.rs` 三条既存 `items_after_test_module` warning 未在本轮扩展处理。负向对照均按预期失败。Linux 侧为真实 cgroup/PSI，不用 mock 或静默 skip 代替。
- 验收矩阵主体使用 `f2e0872` 二进制；严格 owner cap 的最终源码 `3181e71` 另有 mixed/256 压力、完整下载和五轮性能复核。未把旧二进制上的六组结果冒充最终源码结果。
- 未 push、未发版、未部署生产；无关 dirty changes 未触碰。

## 证据

- `docs/evidence/memory-pressure-2026-10-29-round2/README.md`
- `source-metadata.json`
- `request-pressure-summary.json`、`accepted-completion-summary.json`
- `final-resort-triggers.log`、`fast-ramp-failure-summary.json`、`fast-ramp-oom-kernel.log`
- `paired-timing-summary.json`、`final-fair-paired-summary.json`、`final-fair-native-summary.json`
- `native-summary.json`
- 负向对照与全量日志见同目录。
