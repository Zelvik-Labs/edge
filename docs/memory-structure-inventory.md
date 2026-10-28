# Edge 内存结构清单与治理缺口

日期：2026-10-28。基于四路只读代码盘点合成（治理与账本 / 连接与协议 / 缓存 / 遥测日志防御控制面）。
行号对应盘点基线 `0a9499b1b1be33cc20deea424f1cc60111a1ea86`，并不随之后的实现改动自动更新。§0–5 描述该基线；§6 是实施中的契约。

路径缩写：`GOV`=src/memory_governor.rs，`REC`=src/memory_reclaim.rs，`SHED`=src/memory_shed.rs，
`TIC`=src/memory_ticket.rs，`CH`=src/cache_hybrid.rs，`ST`=src/metrics/storage.rs，`CM`=src/cache_manager.rs，
`PROXY`=src/proxy.rs，`DEMUX`=src/quic_udp_demux.rs，`UDP`=src/udp_proxy.rs，`L4REG`=src/l4_connection_registry.rs，
`L4DEF`=src/l4_defense.rs，`STATS`=src/rpc/stats.rs，`NODE`=src/rpc/node.rs。

## 0. 结论摘要

1. **resident ledger 只覆盖 5 类缓存结构**（`GOV:269-289`），其余约 40 个长生命周期结构没有字节级账本。
2. 有**字节预算**的结构包括连接准入、relay、UDP 队列、tcp queue、cache read、request workspace、
   resident ledger、disk ledger 和 telemetry spool；metrics gauge 仅供观测，不是预算。其余许多结构只有
   条目数上限，字节靠 entry×固定经验值估算（key 字符串、HashSet 内容、allocator 开销都不计）。
3. 代码里真实存在**随时间线性增长、无上限**的结构，至少 4 个（见 §3），其中
   `start_metric_stat_reporter` 的 `values_cache`（`STATS:1054`）是最确定的一个。
4. L1 正文缓存（默认 512 MiB、`auto_budget=false`，`CM:88`）**完全不在账本内**，是「账本外 RSS」的最大单块。
5. 现有测试以形状/边界为主；**没有**「长跑后结构回到基线」的集成测试，也没有按类别的增长断言。

## 1. 记账现状

| 层 | 内容 | 位置 |
|---|---|---|
| 快照 | 250ms 周期读 `/proc/meminfo`、`smaps_rollup`、cgroup v1/v2、PSI、fd；压力分级 Normal/Elevated/High/Critical（<20% / <10% / ≤2% 或 ≤64MiB，另有 memory.high、RSS≥90%、PSI 下限） | `GOV:4535-4577` |
| 预算 | connection 45%、keepalive 12%、cache 25%、bloom 5%、state 8%、event queue 2%、pinned 预留；全部随 available/total 动态 | `GOV:495-500, 3987-3998` |
| 字节账 | 连接准入（估算）、zero-copy relay、UDP queued、tcp queue、cache read、request workspace、shared account、disk ledger、telemetry spool（实测压缩字节） | `GOV:1367-1520`, `telemetry.rs:500-540` |
| resident ledger | 仅 CacheMetadata/CacheAccessLog/SurrogateIndex/BloomFilter/NegativeCache；owner 表上限 4,000,000，5 分钟对账 | `GOV:269-289, 315-322, 2754-2800` |
| 估算及计数 | metrics trackers 有观测性字节估算；WAF 各表、L4 表、日志队列、kernel 队列、regex 缓存主要有条目数 | 见 §2 |
| 无记账 | L1、keepalive 池、H3 池、UDP/QUIC 路由与会话表、client-agent、IP list、ConfigStore、geo cidr_sets 等 | 见 §2、§3 |

`unaccountedAnonRssBytes = anon RSS - tracked`（`GOV:2951-2953`），tracked 汇总 resident、
连接/relay/队列/cache-read、workspace 及 metrics 估算 gauge（`GOV:2805-2814`）。估算可能重复计算共享对象，
从而压低残差；**L1、keepalive 池、路由表、会话表都不在其中**。

## 2. 结构总表（按类）

列：上限（来源）｜回收｜账本/上报｜测试缺口。「—」＝无。

### 2.1 连接 / 会话 / 协议

| 结构 | 位置 | 上限 | 回收 | 账本/上报 | 测试缺口 |
|---|---|---|---|---|---|
| L4 连接注册表（H1/H2/SNI） | `L4REG:137-204` | 由准入类与 fd 预算约束，无自身 cap | guard Drop、drain | ticket 字节计入 workspace；无 resident | 无长跑/泄漏测试 |
| Pingora 上游 keepalive 池 | `pingora-main/pingora-pool:187-433` | keepalive 预算/16KiB × threads | LRU + 120s idle | 无池占用/字节上报 | 无占用测试 |
| 上游 H3 连接池 | `origin_h3_pool.rs:8-53` | 4,096 条、idle 120s | 仅 take/offer 时 sweep | — | 无池满/无流量不 sweep 测试 |
| 源站 H3 能力表 | `origin_h3_state.rs:6-154` | 131,072 / 高压 16,384 | 6h idle，调用路径 sweep | — | 无长跑测试 |
| 源站失败状态表 | `origin_state.rs:8-125` | 512，满即任意移除 | 满时淘汰 | — | 无测试模块 |
| 源站 inflight 计数表 | `origin_state.rs:134-205` | **UNBOUNDED**（无 cap、entry 不删） | — | — | 无 |
| Client-Agent IP 表 | `client_agent.rs:78-204` | **UNBOUNDED**（仅配置加载时 clear） | — | — | 无 |
| UDP 会话表 | `udp_proxy.rs:956-2136` | 准入类 | 60s idle、Closed、listener 移除 | udpQueuedBytes | 无收缩测试 |
| UDP 创建中会话表 | `udp_proxy.rs:957-1018` | **UNBOUNDED**（瞬态） | guard Drop | — | 无 |
| UDP pending 队列表 | `udp_proxy.rs:961-1581` | 4,096 session × 8 datagram | 创建结束 remove | 字节计入 UDP queue | 无 cap 测试 |
| UDP recent upstream port | `udp_proxy.rs:296-322` | 65,536、TTL 120s | 30s sweep | — | 无 |
| UDP DNS cache/inflight | `udp_proxy.rs:614-664` | cache 4,096/30s；inflight **无 cap** | moka TTL / guard | — | 无 |
| QUIC 主路由表 | `DEMUX:66,1401-1431` | 每端口 route_limit（1..1e8） | idle 180s/压力、Closed、5s maintenance | 仅计数用于压力 | 无 cap/收缩测试 |
| QUIC CID 表 | `DEMUX:67,418-472` | 无独立 cap（随 route×CID） | remove/cleanup | — | 无 close 后收缩测试 |
| QUIC pending 重组 | `DEMUX:69-94,324-405` | 每端口 128..2048；字节 512KiB..64MiB | 超时/合并/成功释放 | used 仅本地原子 | 无 cap 测试 |
| 新路由 per-IP 窗口 | `DEMUX:72,279-306` | 16,384 IP | 5s retain | — | 无 |
| H3 datagram 队列 | `DEMUX:71,97-125` | 1,024..65,536；字节 512KiB..64MiB | Drop 释放 | 本地原子 | 无满载测试 |
| 请求限额绑定/计数 | `PROXY:589-631,2247-2458` | 1,000,000 / 高压 131,072；TTL 180s | 30s sweep | — | 无 cap/TTL 测试 |
| HTTP parse mark | `PROXY:599-708` | 262,144 / 高压 65,536；TTL 30s | take/TTL/满时丢弃 | — | 无 H2 churn 测试 |
| 上游 TLS connector | `tcp_proxy.rs:170-194` | 512 | 满时移除 75%；压力 50/0% | — | 无 |
| TLS session cache | rustls 默认 256/config | 每 ServerConfig/ClientConfig 256 | rustls 内部 | — | 无 |

### 2.2 缓存

| 结构 | 位置 | 上限 | 回收 | 账本/上报 | 测试缺口 |
|---|---|---|---|---|---|
| L1 正文缓存 | `CH:2791-2846` | 默认 512 MiB（`CM:88`），不跟 governor | TinyUFO + 压力 reclaim | **不在 resident ledger** | 无默认预算/自动淘汰测试 |
| 元数据索引 | `ST:1324-1759` | 1,000,000 条，单条 ≤256KiB | purge/purger/过期 | CacheMetadata（估算字节） | 无 1M 边界测试 |
| 访问日志 | `ST:1288,1992-2021` | 262,144 | 随元数据删除 | CacheAccessLog | 无 262k 边界测试 |
| 墓碑 | `ST:95,1455-1520` | 1,000,000 | 版本推进 | CacheMetadata | 无 |
| Surrogate 索引 | `CH:2541-2732` | 1M/131k tags，4M 成员 | 删除/trim/Critical | SurrogateIndex | 已覆盖退款 |
| Bloom | `CH:3211-3550` | bloom 预算；初始 1.21MiB 不 charge | rotation/shrink/reset | BloomFilter（首次 insert 才同步） | 无「到达预算停止增层」 |
| Negative cache | `CH:3547-3618` | 1M..16M，受 governor | TTL/cleanup/drain | NegativeCache | 已覆盖退款 |
| 共享/WAF regex 缓存 | `bounded_regex_cache.rs:9-39` | 16,384 / 预算推导 | moka TTL + 压力 | — | 已覆盖部分回收 |
| Mace 持久层内存缓存 | `ST:97-175` | tier：WAL 1-4MiB、LRU 8-32MiB 等 | Mace 内部 | disk ledger | 无 |

### 2.3 遥测 / 指标

| 结构 | 位置 | 上限 | 回收 | 账本/上报 | 测试缺口 |
|---|---|---|---|---|---|
| MetricAggregator ×2 | `metrics/aggregator.rs:178-340` | key 数 C=65,536..4,000,000；单 entry request_samples 同 C（最坏 C×C） | flush/restore | 30s 估算 gauge，不参与限流 | 无 attrs 长跑/C×C 测试 |
| Daily/UniqueIp/TopIp tracker | `metrics/daily.rs`, `top_ip.rs` | C | 窗口 flush/cleanup | gauge | 无启动大 Vec、跨日压力 |
| `RuntimeDistinctIpTracker` | `metrics.rs:539-604` | 每 server IP 数按 servers.len 分摊 | 无 TTL | **不计入 metrics_state_bytes** | 无 |
| `METRICS.servers` | `metrics.rs:863-873` | **UNBOUNDED**（生产无删除） | — | — | 无 |
| HttpDimension 队列 | `metrics.rs:451-537` | E/2/512，2,048..1,000,000 | 满即丢 | — | 无 |
| GEO/UA LRU | `metrics/analyzer.rs:61-189` | 预算推导 / 64 shard | LRU + 压力清空 | — | 无容量测试 |
| **metric reporter `values_cache`** | `STATS:1054` | **UNBOUNDED**（key 含 time_key） | 无 | 无 | 无 |
| daily reporter `pending_stats` | `STATS:756-833` | **UNBOUNDED**（失败回填） | 成功即清 | 无 | 无 |

### 2.4 日志 / 上报

| 结构 | 位置 | 上限 | 回收 | 账本 | 测试缺口 |
|---|---|---|---|---|---|
| access/node 日志队列 | `logging.rs:16-97` | E 推导，2,048..1,000,000 / 512..100,000 | 满即丢 | entry 估算 | 无满载测试 |
| NODE_LOG_THROTTLE | `logging.rs:183-281` | 4,096，**entry 永不删** | — | 无 | 无 |
| local log writer 队列 | `main.rs:53-180` | E/8/1024 | 满即丢、文件轮转 | 无 | 无 |
| telemetry spool | `telemetry.rs:115-540` | 磁盘自适应；按压缩字节 | ack/quarantine | 磁盘字节实测 | 已覆盖主要路径 |
| telemetry access/node buffer | `telemetry.rs:1851-1940` | 512/128 条/批 | flush；失败丢弃 | 无 | 无失败缓冲测试 |
| probe OUTBOX | `probe.rs:88-203` | **UNBOUNDED**（失败回填） | 成功 drain | 无 | 无 |
| 旧 LogUploader/NodeLogUploader | `log_uploader.rs` | 有界 | — | 无 | **未接线**（生产不跑） |

### 2.5 安全防御

| 结构 | 位置 | 上限 | 回收 | 账本 | 测试缺口 |
|---|---|---|---|---|---|
| WAF 7 个 scoped map | `firewall/state.rs:467-577` | 每表 C_w=65,536..8,000,000（7 表合计约 7×C_w） | 过期/满时淘汰、60s GC | 无 | 无 7 表合计测试 |
| network/range DashMap + 快照 | `firewall/state.rs:474-2510` | 同 C_w/表 | GC/重建 | 无 | 无容量测试 |
| ip_limiters | `firewall/state.rs:492-1751` | C=32,768..32,000,000 | 90s idle | 无 | 已覆盖部分 |
| server_limiters | `firewall/state.rs:492,1558-1751` | **无 entry cap** | 90s idle | 无 | 无 |
| WAF counters | `firewall/state.rs:375-1843` | C=16,384..16,000,000，满 fail-open | 7 天 stale | 无 | 无 |
| CC counters | `firewall/matcher_plus.rs:509-1200` | 1,000,000 / 压力 131,072，满 fail-open | 7 天 stale | 无 | 无 |
| ip bw / candidate | `firewall/state.rs:505-2700` | C 推导 | idle sweep | 无 | 无 |
| firewall PENDING | `firewall/persistence.rs:78-335` | cap/4，4,096..2,000,000 | 写盘成功清空 | 无 | 无失败联合测试 |
| kernel sync 队列 | `firewall/kernel.rs:156-710` | size_of 推导 | 满清空 + 全量 reconcile | 无 | 无 |
| IP 名单 items | `firewall/lists.rs:80-292` | **UNBOUNDED**（控制面决定） | 仅整表删除 | 无 | 无 |
| L4 aggregate | `L4DEF:176-572` | `prefix_counts` 有 cap；`prefixes/distinct_ips` **无 entry cap**（10s 窗） | 10s sweep | 无 | 无 |
| L4 exact counter | `L4DEF:593-665` | (S/8)/256，无 max | 10s sweep | 无 | 无 |

### 2.6 控制面 / 运行时

| 结构 | 位置 | 上限 | 回收 | 账本 | 测试缺口 |
|---|---|---|---|---|---|
| ConfigStore 各代 map | `config.rs:328-1720` | **无字节上限**（decode budget 只管解析） | 写锁替换，Arc 释放 | 仅 json_bytes gauge | 无旧代 Arc 释放测试 |
| parent_pressure | `config.rs:1215-1229` | **UNBOUNDED**（60s 逻辑过期，entry 不删） | — | 无 | 无 |
| published_fingerprints | `config.rs:333,506-522,1667-1720` | 全量 apply 剪枝；增量路径不剪 | — | 无 | 无 |
| Health registry | `health_manager.rs:9-250` | 按活跃 LB；Weak | 1s retain | 无 | 已覆盖 Weak |
| Geo snapshot / 下载缓冲 | `geo.rs:317-408`；store.rs | 单 component ≤128MiB；**component 数与下载期总量无上限**；cidr_sets 全量进 RAM | ArcSwap 替换 | 仅 MMDB 字节 | 无激活/旧代释放测试 |
| PatchJournal.active_payload | `node_control/patch.rs:25-120` | 始终 1 份，无显式大小上限 | 替换 | 无 | 无 |
| RPC stream 通道 | `rpc/stream.rs:281-492` | 1..100 条，命令并发受准入 | 满即丢 | 准入估算 256KiB | 无 |

## 3. 无界 / 弱边界清单（按「随运行时长增长」的确定性排序）

| # | 结构 | 位置 | 增长条件 | 备注 |
|---|---|---|---|---|
| 1 | metric reporter `values_cache` | `STATS:1054`（读写 1020/1027） | 每个时段 × 指标项 × key 组合都插新 key；配置项删除也不清 | **已复核：全文件无删除** |
| 2 | probe `OUTBOX` | `probe.rs:186-189` | 上报失败整批回填 | 无 cap |
| 3 | daily reporter `pending_stats/pending_domain_stats` | `STATS:756-758,828-833` | telemetry enqueue 失败 | 注释明说不过期 |
| 4 | `METRICS.servers` + `RuntimeDistinctIpTracker` | `metrics.rs:863-873` | 每个出现过的 server_id；生产无删除 | 受 server 数 |
| 5 | `origin inflight` 计数表 | `origin_state.rs:134-205` | 每个出现过的 origin_id，entry 不删 | 受 origin 数 |
| 6 | client-agent IP 表 | `client_agent.rs:78-204` | 每个见过的客户端 IP，仅配置加载时清 | 无 cap |
| 7 | `parent_pressure` / `published_fingerprints` | `config.rs:1215-1229` / `1667-1720` | 父地址数 / 增量 apply 不剪枝 | 基数小 |
| 8 | `GlobalIpListManager.items` | `firewall/lists.rs:80-292` | 控制面下发多少就存多少；过期条目无 GC | 控制面侧可控 |
| 9 | L4 `prefixes`/`distinct_ips` | `L4DEF:176-181,545-572` | 10s 窗内不同前缀/IP 数 | 有时间窗 |
| 10 | UDP `inflight_sessions` / DNS inflight | `udp_proxy.rs:957-1018,592-612` | 并发创建/解析键数 | 瞬态 |
| 11 | Geo 下载期 `BTreeMap<Component,Vec<u8>>` | `geo.rs:317-338` | component 数量 | 单 component 有上限 |

## 4. 记账缺口（治理器看不见的真实占用）

1. **L1 正文缓存**：默认 512 MiB，只按 weight 统计，不进 ledger、不进 `unaccounted` 归因。
2. **keepalive 池 / H3 池**：只有池大小预算，没有实际占用与字节。
3. **QUIC/UDP 路由与会话表**：只有条目上限与少量 used 原子，不上报。
4. **WAF/L4 各表**：只有条目数上限，字节按经验值估算；key 字符串与 HashSet 内容不计。
5. **metrics trackers / aggregator**：30s 一次的估算 gauge，不参与准入；`request_attrs` 不计。
6. **ConfigStore / geo cidr_sets / IP 名单**：完全无字节账。
7. **TLS session cache（rustls 默认 256/config）**、**H3 retry buffer（64KiB/请求）**、**UDP 每 worker 65KiB 接收缓冲**：无账。

## 5. 分类测试矩阵

现有资产：
- `tests/memory_compare.rs`：按类别 flood 打 RSS 增量（WAF scoped、metrics 基数等），**只打印不断言**。
- `tests/memory_reclaim.rs`：Critical 回收后 RSS 下降（regex fixture）。
- `tests/memory_governance.rs`：预算/准入/relay 边界。
- `tests/oom_audit.rs`：结构有界性与「预算未接线」记录。
- `tests/config_sync_memory.rs`：热更新多轮 RSS 收敛。

计划分为 6 类测试（每类一个 driver，统一汇总表）：

| 类别 | 驱动 | 断言 |
|---|---|---|
| T1 结构有界性 | 直接构造每个 map/queue，灌到 cap+1 | 不超上限、淘汰符合策略、fail-open/closed 行为符合设计 |
| T2 长跑增长 | 按类别循环产生/消费（连接 churn、QUIC 迁移、UDP session、日志、指标） | N 轮后结构计数与 RSS 回到基线 ±阈值；`values_cache` 等无界结构必须先修复 |
| T3 失败路径 | 注入 enqueue/上报失败 | pending/OUTBOX 有界、丢弃有计数、不无限回填 |
| T4 记账一致性 | charge/refund/reconcile、L1/元数据删除 | ledger 余额回零；`unaccounted` 不随时间单调增 |
| T5 压力回收 | 构造各等级压力 | 对应结构被回收；RSS 变化记录（允许 allocator 不归还，但结构计数必须下降） |
| T6 状态暴露 | RPC snapshot | 关键结构计数存在且与内部一致（新增字段后生效） |

落地方式：把 `memory_compare.rs` 扩成 assertion 版本（Linux-only），新增 `cargo xtask memory-audit`
运行 T2/T3 长跑并输出表格；CI 只跑缩短版（30–60s），长跑留在本地/RKE dev。

## 6. 统一资源管理契约（实施中）

此清单记录旧实现的缺口；下面是实施目标，不能把模型测试或估算字段当成已经生效的限额。

- 连接建立、已建立连接的必要缓冲和控制面分别有不可借出的硬保留；安全执行状态独立于可丢弃的指标与缓存。
- 只有 Normal 且物理 headroom 观测新鲜时，低优先级结构才能借用扣除硬保留、安全余量、待分配/未确认承诺和已建立连接未来工作集后的剩余空间。逻辑退款不会证明内存已经归还；活跃 `Arc` 和 allocator 保留页仍受后续物理观测约束。
- 所有外部输入驱动的扩容都要在分配前取得许可或受可证明的硬上限约束；压力回收只作为后备。Mace 和磁盘操作不进入请求与样本采集热路径。
- 状态必须区分实测队列/permit 字节、容器估算、条目数上限与 RSS 残差。估算可能重复计算共享对象，也不包括 allocator 的全部开销，不能当成 RSS。
- 指标生成、本地持久入队和远端确认是不同状态；入队失败不能推进 baseline，quarantine 和延迟/乱序确认不能回滚后续 baseline。只有低优先级遥测与分析可按获批策略受控丢弃，并记录原因和 gap/loss。

`BorrowingInputs::borrowable_bytes` 当前只提供可测试的计算模型，**尚未接入准入与回收**；`MetricAggregator::memory_estimate` 仅提供进程内按需快照，**尚未接入状态、限制扩容或跟踪已 flush 的批次**。现有周期性 gauge 仍不计动态属性。`BackgroundWork` 同时用于配置应用和缓存工作，不能不区分调用方就归到可丢弃资源。后续阶段必须把分配、借用和批次生命周期接到实际许可账本，完成验证后再更新本节。

已落地的边界：probe outbox 以 16,384 条 / 64 MiB 同时约束排队与正在写 Mace 的批次；失败只回填未持久化的记录，追加任务丢失会计入受控丢弃；flusher 不再随探测计划切换而中止，丢弃计数经 node status 的 `probeOutbox` 暴露。溢出策略是保留队列中较旧的观测、拒绝新观测，而不是无界回填。

metric 与 daily 的按项所有权：metric 报表失败后不再 restore 整个聚合样本，而是把未持久化的 per-server 请求放进 4,096 项 / 16 MiB 的有界 pending 队列，按原始 `time`/`item`/`version` 重试；超过上限按行计入受控丢弃并写 gap。daily 请求按 wire cap 分块，失败只保留未持久化行；pending server 行与 domain 行分别限额（各 65,536 行、合计 64 MiB），保留较旧行并写 gap。上述上限只在 spool 完全拒绝写入时触发，不是正常路径的容量计划。

MetricAggregator 现在同时受 cardinality cap 与 per-aggregator 字节预算约束（`state_budget/16`，16 MiB–512 MiB）。每条 entry 的收费包含其 flush/restore 临时副本；`request_samples` 在插入前按属性字节做原子 CAS 计费，flush 退款、restore 重新计费并可在预算不足时拒绝。`approximate_bytes` 改为读取该账本，不再每 30 秒遍历全部 entry 估算。

distinct-IP tracker 原先按创建时的服务器数分配独立容量：每个新服务器再拿一份 `cardinality_state/8` 的切片，旧服务器切片不回收，总容量因此随服务器代数按调和级数增长。现在所有 tracker 共用一个进程级原子条目预算（`state_budget/8` 按每项 64 字节估算，16,384–16,000,000 项），插入前计费、重复项退款、tracker 释放时按实际条目退款；预算耗尽会计入既有 `metricsCardinalityDropped`。该结构仍没有 TTL，先到的高流量 server 可以占满全局预算，因此其他 server 的新 IP 会被拒绝；这是指标分析数据的受控丢弃，不改变连接、控制面或安全状态。

`parent_pressure` 的样本在 60 秒后逻辑失效，但旧实现让条目永久驻留。现在更新路径每 60 秒至多触发一次 `retain`，删除已过期地址；清理不在请求路径，且只影响父节点压力反馈，不改变选路或连接状态。

L1 正文缓存的权重估算字节现在作为 governor gauge 上报，并从 `unaccountedAnonRssBytes` 的已归因部分扣除；node status 同时给出估算字节与配置预算。该字段只做归因，不授予容量、不进入 resident ledger；L1 仍由缓存管理器以固定默认预算创建，尚未按 governor cache budget 自动调整。

## 7. 非代码因素

- 宿主 `transparent_hugepage=always` 会放大 RSS（10-22 实测 anon_thp 占 40–75%），并让
  “清结构但 RSS 不降”成为常态；网络节点建议 `madvise`。
- mimalloc 选项已在 `REC:74-77` 调整（abandoned purge、max segment reclaim、arena purge mult），
  `mi_collect` 只在调用线程生效（`REC:491-550`），跨 worker 归还依赖轮转。
