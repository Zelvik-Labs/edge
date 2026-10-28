# 2026-10-29 内存治理验证

状态：已完成本轮代码审查、确定性复现、真实 cgroup 压力与五轮配对性能测量。
结论是连接优先/续传保护**未通过**，L2 小对象所有权与热路径性能通过；
不得据此宣布内存治理验证矩阵完成。

## 代码审查与确定性复现

基线为 `0a9499b1b1be33cc20deea424f1cc60111a1ea86`，候选为
`2c6b2e19c7a27ca12f21d392ba6640b602f0774c`。候选保留此前已验证的治理修改。
本轮另有压缩 L2 取消生命周期修复及测试，尚未包含在 Linux 对比二进制中；
对比使用动态回源路径，不经过该 L2 解码路径。

### Established reserve 与共享账户

- Established reserve 为 `1024×16 KiB + 1024×24 KiB + 512×32 KiB = 56 MiB`，
  小预算下取连接预算的一半。Control reserve 为 `32×64 KiB + 16×256 KiB + 64 KiB = 6,356,992 B`。
- 连接类别的估算预算与 shared account 是两道准入约束，各自检查同一请求，
  不是从一份可用余额中连续扣两次 reserve。新增八线程饱和测试验证并发 pending grant
  到达建连边界后，剩余空间恰好可容纳一份 established 和一份 control reserve；
  再多一个字节建连被拒绝，全部 pending permit 释放后余额恢复。
- CAS 争用的是 `account_outstanding_bytes`。pending 转 committed-unconfirmed 时，
  outstanding 不变；pending drop 立即归还；committed drop 只结束归属，
  扣减要到真实内核采样后结算。审查和上述固定快照测试未复现超额批准或重复扣 reserve。
- 56 MiB 仅阻止新连接耗尽这一余量。已有连接上的**新** H2/H3 stream、origin connect
  与已接受请求的后续资源使用相同 Ordinary 优先级，不能据此声称续传资源受到独立保护。

### 单连接隔离失败

`memory_ticket::tests::audit_known_gap_single_bucket_exhausts_global_workspace`
证明：单个 bucket 通过 2 MiB RequestBodyWaf 和 16 KiB H2 spends 耗尽 64 MiB
全局 workspace，使独立 victim bucket 无法取得一个 16 KiB stream；此时 shared account
仍有余量。全部持有者释放后 workspace 回零。这是当前缺口的复现测试，通过表示
复现成功，不表示隔离正确。

`audit_known_gap_pooled_h2_streams_bypass_global_count_limit` 证明 pooled 路径可持有
全局上限加一个 stream，而 `estimated_h2_streams` 与 connection admission estimate
仍为零。workspace 总字节约束有效，但不能替代全局 stream 计数或每连接公平性。

真实协议路径还有两种存活风险：

- `http_proxy_manager.rs` 在 per-connection 或 global H2 stream admission 失败时 `break`，
  退出整个连接的 accept loop，已接受 stream 也会受连接关闭影响。
- `memory_shed.rs` 持续 Critical 的年龄阶梯最终降为 0，取消所有注册 H1/H2 连接，
  并通过 H3 owner hook 关闭 H3 连接。该行为有意避免 OOM，但不保证已接受响应完成。
  本轮不通过添加 UDP 强制断连来扩大覆盖面。

### L2 Bytes 生命周期

- 小 L2 内存命中在读文件前取得 admission，`try_reserve_exact` 和 boxed slice 规范化
  使 Rust 报告的 capacity 对齐所收费长度。它不是 allocator usable size 或物理 RSS。
- `Bytes::from_owner` 持有 shared permit 与 cache-read lease；handler、原始 chunk 释放后，
  最后一个 downstream slice 仍持有收费，最后引用释放后归还归属与专用 ledger。
- 修复压缩命中取消缺口：将 permit 与输入一起移入 `spawn_blocking`，在同一 closure
  内完成解码、capacity 规范化与 Bytes handoff。取消外层 lookup 后后台任务及其结果
  继续持有收费，直到工作完成且最后 owner 释放。
- 缓存模块最终 54 项通过；本地 `test --lib` 全量为 1090 passed、2 ignored、0 failed。
  helper 取消测试的提前退款负对照失败（期望收费 64，实际只剩 32）。
  新增真正取消 `FileStorage::lookup` 的测试，回退生产调用点为旧的 lookup-local permit
  后失败（期望 105，实际 0），恢复修复后通过；恢复源码的 lib/fixture clippy 通过。
  解码失败与长度错误路径也验证退款。独立只读审查未发现生产修复正确性缺陷。
- 生产 streaming zstd 编码器可产生约 2 MiB window；小对象解码 workspace 由 4 MiB
  admission estimate 覆盖，不能把对象长度 ledger 当作完整分配量。流式大对象解码
  尚无等价 workspace permit，是剩余缺口。不能随意限制 window 后把现存正常缓存删为损坏。

## Linux 测试方法

专用测试机 `zelvik-dev-43`，所有代理、源站与控制端口仅监听 loopback。
真实使用 `HttpProxyManager`、`Http3ProxyManager` 与 `EdgeProxy`，不是简化 bench proxy。
每轮启动全新 systemd cgroup：256/512 MiB MemoryMax、90% MemoryHigh、swap=0、
CPUQuota=200%、代理 CPU 0–1、LimitNOFILE=65536。触碰每一页的匿名 ballast
把 `memory.current` 依次推近 50/70/85/90/94/97%；记录 current/peak/events/PSI、
RSS/PSS、已接受长响应进度以及新请求成功数/P99。失败请求不计入成功延迟，
未尝试请求和失败状态单独保留。

区分两种压力场景：已接受 H1/H2/H3 长响应期间继续创建新请求，以及纯续传、不创建新 stream。
每种内存限制重复三次。另以单条 H2/H3 攻击连接创建有界数量的慢 stream，
观测独立 victim 连接；此网络场景不等同于确定性测试的 WAF workspace 饱和。

性能对比采用同一 fixture、相同 Cargo.lock、Rust 1.99.1、opt-level=2/debug=0/
incremental=0/codegen-units=16。五轮交替 AB/BA，每轮独立进程。分配计数单独运行，
计时期间关闭计数。native 客户端使用 wrk/h2load/bench-h3-load，CPU 2–3；源站 CPU 4–5。
H2 每连接 150 请求，低于现有 200 streams/30s churn 关闭策略。
保留软件 task-clock、调度、缺页、futex 计数及独立 CPU 采样。虚机不支持硬件 PMU，
不能声称直接测到了 cache-line bouncing、硬件 cache misses 或所有锁的等待时间。
Python 单线程源站与客户端也可能限制吞吐；吞吐结论必须结合 CPU 使用和多轮离散程度。
额外运行直接访问源站的 wrk 样本，识别源站吞吐上限。分配计数另做三轮 AB/BA，
避免把单次 allocation 样本误认为稳定开销。

Python 客户端的五轮 AB/BA 热路径配对（每轮每协议 4000 请求，全部成功）：

| 协议 | baseline RPS 中位数（范围） | candidate RPS 中位数（范围） | baseline P99 中位数 | candidate P99 中位数 |
|---|---|---|---|---|
| H1 | 2607.3（2574.9–2625.7） | 2783.6（2703.6–2788.0） | 10.47 ms | 9.56 ms |
| H2 | 1293.9（1263.8–1325.5） | 1312.0（1255.0–1340.9） | 13.49 ms | 13.23 ms |
| H3 | 447.1（444.7–456.9） | 451.9（448.3–469.5） | 41.15 ms | 40.60 ms |

H1 五轮 candidate 全部高于 baseline 的范围；H2/H3 的范围重叠，不能声称稳定提升，
只能说在本负载下未观察到吞吐/尾延迟回归。H1 的差异也可能包含代码布局或
构建顺序效应，治理机制与布局无法从这组数据分离。
分配计数另做三轮 AB/BA（每轮 1000 请求，全部成功）：H1/H2/H3 的 candidate
分配次数与 baseline 中位数相差分别约 +0.06%/+0.02%/-0.001%，无治理路径新增
分配的证据；计数包含后台线程，不是严格的每请求分配数。
native 客户端（wrk/h2load/bench-h3-load）的五轮配对另表统计，包含 per-protocol
task-clock、上下文切换、futex 计数与 CPU 采样，见下节。

native 配对（每协议五轮 AB/BA，代理 CPU 0–1，客户端 CPU 2–3，源站 CPU 4–5；
每轮每协议全部 2xx，失败/超时为 0）：

| 协议 | baseline RPS 中位数（范围） | candidate RPS 中位数（范围） | baseline P99 | candidate P99 | baseline CPU ms/req | candidate CPU ms/req |
|---|---|---|---|---|---|---|
| H1 | 2513.9（2495.9–2533.7） | 2817.5（2724.2–2933.9） | 21.08 ms | 17.94 ms | 0.789 | 0.704 |
| H2 | 2009.6（2004.7–2056.8） | 2329.8（2312.7–2351.3） | 24.76 ms | 19.65 ms | 0.974 | 0.839 |
| H3 | 2400.8（2375.5–2405.2） | 2823.2（2786.4–2846.6） | 23.73 ms | 19.67 ms | 0.828 | 0.702 |

每 1000 请求的软件计数器中位数：

| 协议 | context switches baseline→candidate | futex baseline→candidate |
|---|---|---|
| H1 | 838.7 → 748.2 | 390.2 → 348.5 |
| H2 | 1038.5 → 897.7 | 612.3 → 559.6 |
| H3 | 905.0 → 788.4 | 539.28 → 539.36 |

直接访问测试源站的 wrk 为 20,650 RPS，远高于代理 H1 的 2.5–2.8k RPS，说明这组
H1 数字主要由代理 2 CPU 配额和客户端协议开销决定，而非 Python 源站上限。
候选在五轮中位数上更快、每请求 CPU 更低，未观察到治理改造引入吞吐或尾延迟
回归；但 baseline→candidate 包含整个已提交治理集（0a9499b..2c6b2e1），
不能把差异逐项归因到某个机制。CPU 采样中热点仍是 mimalloc 的 page malloc/free、
libc 内存操作和内核 TCP wakeup；没有出现 governor/admission 的新主导热点。
硬件 PMU 不可用，futex 计数只能作为软件可观测的争用代理，不能证明 cache-line
bouncing 为零。

复现入口：`scripts/perf/memory_pressure.py`、`run_memory_pressure.py`、
`run_memory_pressure_native.py`、`summarize_memory_pressure*.py`；fixture 为
`src/bin/memory-pressure-node.rs`。文本证据保存在
`docs/evidence/memory-pressure-2026-10-29/`，原始 JSON/perf/日志仍在专用测试机。

## 真实压力初步实测

带新请求的 256/512 MiB 各三轮完成，六轮均无 `max/oom/oom_kill` 事件。
`memory.peak` 分别为 250.07–255.77 MiB 和 499.26–503.17 MiB，已经接近硬限制；
`memory.high` 每轮累计 4,253–14,641 次。最重的一轮 cgroup PSI full avg10 达 86.50%，
不是只修改 synthetic Critical 标志。

但连接存活未通过：六轮最终已接受 H1/H2/H3 长响应全部中断。512 MiB 三轮在
85% 目标阶段新请求成功数均降为 H1/H2/H3 各 0/50；H2 原有长响应同时丢失。
持续 Critical 后 H1/H3 也被年龄阶梯主动 drain。256 MiB 同样出现拒绝和续传中断。
纯续传另做 256/512 MiB 各三轮：六轮均在 94% 目标阶段 H1/H2/H3 全部失活，
此前 85%–90% 阶段仍有进度，与 `memory_shed` 的 0ms 年龄档吻合。
这六轮均无 `max/oom/oom_kill`；peak 为 250.48–251.72 MiB 和 497.31–499.67 MiB。
两批十二轮最终连接存活率均为 H1/H2/H3 各 0/3（每种 cgroup 限制、每种场景）。
因此既有请求继续转发的保护失败可稳定复现，并不需要同时创建新 stream。

压力终态统计（请求压力批 3 轮 + 纯续传批 3 轮，每档限制各 3 轮）：

| 限制 | memory.peak | memory.high | RSS / PSS（请求压力批 97% 档） | 长响应存活 |
|---|---|---|---|---|
| 256 MiB | 250.07–255.77 MiB | 4,253–10,458 | 275,888–276,020 / 273,348–273,561 KiB | 0/3 每协议 |
| 512 MiB | 497.31–503.17 MiB | 7,774–14,641 | 521,004–530,212 / 518,504–527,757 KiB | 0/3 每协议 |

所有批次 `max/oom/oom_kill` 均为 0；`memory.peak` 接近硬限制，且 `memory.high`
在每轮持续触发。进程 RSS 可高于 cgroup `memory.max`（页缓存/共享页归属不同），
所以这里同时给出 cgroup current/peak 与进程 RSS/PSS，不能互相替代。
cgroup PSI full avg10 最高观测 86.50%；宿主 PSI 在首轮后仍有 avg300 余波，
已在后续批次逐轮等待恢复后再启动。

注意两项测试限制：第一批没有等待宿主 PSI avg10 恢复，而 governor 当前读取宿主
`/proc/pressure/memory`；因此部分 fresh cgroup 在 50% 阶段已经呈 High，不能把该批
阶段百分比当作精确的压力阈值标定。后续纯续传/性能批次逐轮等待宿主 PSI avg10 <1%。
第一批 H1 客户端也未充分遵从压力响应里的 `Connection: close`，可能把后续复用关闭
连接算为失败；驱动已修正，此批 H1 成功率不用于性能结论。上述两点不影响内核
压力存在、无 OOM kill、以及长响应被主动 drain 的事实。

单连接 GET 慢流测试中，H2 768 次创建尝试有 100 条成功、668 条客户端或远端拒绝；
独立 victim 的 30 个 H2 和 30 个 H3 请求均成功，既有长响应继续。H3 创建 768 次时
100 条已有响应、667 条仍在等协议/请求处理，直接全局 H3 活跃计数达到 256；victim
仍成功，但 H3 P99 从 H2 攻击阶段的 57.32 ms 上升到 H3 攻击阶段的 118.42 ms。
这是单轮有界慢流现象，不是吞吐回归结论，也没有复现 WAF workspace 饱和。
不能用这次 GET 测试否定确定性测试已经证明的 bucket 字节公平性缺口。

## 结论矩阵

| 项目 | 结果 | 证据 |
|---|---|---|
| 56 MiB reserve 计算、共享账户 CAS | 通过：未复现重复扣除或并发超额批准；8 线程饱和边界精确 | `concurrent_connection_grants_preserve_exactly_one_established_reserve` |
| 新建连接不能消耗 reserve | 通过：ConnectionEstablishment 账户门槛扣除 control+established | `memory_governor.rs` `try_charge_account_with_priority` |
| 已接受请求的续传优先级 | **失败**：续传与新 stream 同为 Ordinary；56 MiB 可被 Ordinary 使用 | 确定性测试 + `priorityReserveEnforced` 文档限定 |
| 单连接 workspace 公平性 | **失败**：单 bucket 可耗尽全局 workspace，victim 无法取得 16 KiB stream | `audit_known_gap_single_bucket_exhausts_global_workspace` |
| pooled H2 全局 stream 计数 | **失败**：可超过全局上限且计数/连接估算仍为 0 | `audit_known_gap_pooled_h2_streams_bypass_global_count_limit` |
| 真实 cgroup 近限压力 | **失败（存活标准）**：12/12 轮最终丢失已接受 H1/H2/H3 长响应；无 OOM kill | 两份 summary + 原始测量 |
| L2 分配前收费/最后引用退款 | 通过（小对象内存命中）；取消路径已修复 | 54 项缓存测试 + lookup 负对照 |
| L2 大对象 streaming decoder workspace | **仍有缺口**：无等价 permit | 代码审查 |
| 配对热路径性能 | 本负载无回归；native 五轮中位数 candidate 更快、CPU/上下文/futex 更低或持平 | timing/native summary |

## 需要用户决策的最小策略点

以下不是本轮擅自修改的行为，而是实测确认需要策略选择的最小集合：

1. **每连接在途 workspace 上限**：`TicketBucket` 没有 `held` 上限，单连接可耗尽全局
   workspace，令其他连接无法取得一个 16 KiB stream。可选的修法是给每 bucket 增加
   在途字节上限（数值需要按连接/协议工作集论证），或在全局 workspace 耗尽时优先
   拒绝大额 WAF 缓冲而保留 stream 小额准入。前者改变并发上限，后者需要定义优先级；
   两者都需要明确批准，不能靠 LRU/TTL 随意删除 client-agent 有效 crawler 豁免。
2. **新 stream 拒绝语义**：当前 global/per-connection H2 stream admission 失败会
   `break` 整个连接 accept loop，已接受 stream 随连接关闭。若目标是“拒绝新 stream
   但保留既有 stream”，需要改成只拒绝该 stream、发送 RST/GOAWAY 并继续循环；
   这会改变现有攻击面语义，需确认。
3. **Critical drain 阶梯**：`DRAIN_AGE_LADDER_MS` 最后一档为 0，持续 Critical 会取消
   所有注册 H1/H2 连接并关闭 H3 连接。若要求已接受响应必须完成，需要给该阶梯设置
   下限或改为只拒绝新分配；UDP 没有安全 drain hook，不能通过强制断连补齐。
   本轮保留现有行为并把失败结果作为证据。

L2 大对象 streaming 解码仍缺少 decoder workspace permit；任何 window 限制都必须
兼容现有 2 MiB 生产 frame，不能把正常缓存误判为损坏。
