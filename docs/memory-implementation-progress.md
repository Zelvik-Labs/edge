# 内存治理实施记录

基线：`0a9499b1b1be33cc20deea424f1cc60111a1ea86`。分支：`feat/edge-memory-governance`。
日期：2026-10-28。本文区分已验证实现与后续目标；不是整体完成声明。

## 已提交

| 提交 | 范围 | 验证 | 限制 |
|---|---|---|---|
| `2aa74b0` | 资源域、硬保留借用计算、按需 aggregator 估算、shared account 状态、基线结构清单 | 库测试 1043 通过 / 2 手动 UDP 用例忽略；新增状态用例独立通过；恢复后 borrowing 2、estimate 3、status 1、logical release 1 通过 | 借用模型尚未强制执行，估算不是字节限额或 RSS |
| `f325810` | 每 60 秒删除 origin inflight 零计数项；递增与清理共用 shard 锁 | origin 3 用例通过；去掉清理后失败；把递增移到锁外后确定性锁测试失败，恢复后 3 通过 | 未限制同时存在的 origin 数量 |
| `ab6a8a6` | shared account 使用单一 outstanding 原子作为 pending/committed 共同门槛，分类迁移不释放额度 | governor 48 + lifecycle 20 通过；恢复旧双计数器后 commit 窗口与交错申请两个用例确定性失败 | 保留预算仍未接入实际分配 |
| `f262790` | 删除按时间键无界增长的 metric `values_cache`，每次从当前样本构建完整行 | metric stat 4 用例通过；接收端忽略 `keepKeys`、ClickHouse 替换表不求和已核实 | 单条 flush 失败仍整批回填；超大指标拆分未做 |
| `e33e58a` | probe outbox 同时约束排队与 in-flight；失败只回填未持久化记录；flusher 独立于计划；丢弃计数与状态 | probe 22、telemetry 31、status 1 通过；三项负向对照分别失败（忽略 in-flight、丢弃剩余批、清除计划中止 flusher） | 旧/新观测溢出策略为保留较旧项；daily pending 与 aggregator 未处理 |
| `bf82d3b` | metric/daily 按 wire cap 分块并只回填未持久化行；metric 以有界 pending 队列按原时段重试，不再 restore 全部样本；daily pending 按行数与字节截断并写 gap | telemetry 33、stats 10 通过；metric/daily 精确尾部、metric 队列上限、daily 截断均有新增用例；丢弃剩余批与跳过截断两项负向对照失败 | metric 失败请求队列 4,096 项 / 16 MiB；daily 65,536 行 / 65,536 域行 / 64 MiB；截断时保留较旧行 |
| `208be7f` | MetricAggregator 增加 per-aggregator 字节预算；entry/属性/request sample 分配前 CAS 计费；flush 退款、restore 重新计费；gauge 改读账本 | aggregator 11 通过；governor 缩放用例断言 16 MiB 下限与 512 MiB 上限；禁用预算与跳过退款两项负向对照失败 | top_ip 与 daily tracker 仍是条目上限；字节拒绝复用 cardinality 计数 |
| `faae6ca` | 所有服务器的 distinct-IP tracker 共用一个进程级条目预算；插入前原子计费、重复项退款、tracker 释放按实际条目退款；node status 暴露已用/预算条目 | metrics 9、governor 512MiB/256GiB 缩放用例、`clippy --lib` 通过；去掉容量检查的负向对照使共享预算用例以 used=3/capacity=2 失败 | 只约束条目预算，不做 TTL；先到的高流量 server 可以占满全局预算，其他 server 的新 IP 会计入 cardinality 丢弃；Linux RSS 未验证 |
| `b4ef26f` | parent pressure 逻辑过期 60 秒后仍永久保留；更新路径每 60 秒至多清理一次，删除过期地址 | config 44 通过；保留过期项的负向对照失败 | 未限制一分钟内新增的父地址数；清理走 Dashboard retain，不在请求路径 |
| `b18ca2e` | L1 正文缓存估算字节进入 governor gauge 与 RSS 归因，node status 暴露估算/预算；不改 resident ledger | L1 归因用例、`clippy --lib` 通过；把 gauge 从 tracked RSS 移除的负向对照失败（64MiB 计为 0） | 只改进归因；L1 仍是固定 512MiB 默认，未与 governor cache budget 对账，未做 Linux RSS 验证 |
| `1261361` | L1 初始预算改为 `min(512MiB, governor cache budget)`；大机器不放大，小机器不超过整个 cache 预算 | L1 预算用例、`clippy --lib` 通过；总是返回 512MiB 的负向对照使 128MiB cache budget 用例失败 | 仍不是动态 resize；运行时 cache budget 变化不会自动缩小 L1，Linux RSS/压力验证未做 |

所有本机 Rust 测试通过工作区 `scripts/local-rust.sh`、mbx、单编译槽运行。未清缓存。未发布、部署或修改 tag。

全量库测试：macOS debug `cargo test --lib` 1059 通过 / 2 手动 UDP 用例忽略；`cargo clippy --lib -D clippy::correctness -D clippy::suspicious` 通过（仅保留既有 `items_after_test_module` 警告）。`tests/memory_governance.rs` 10 通过、`tests/oom_audit.rs` 11 通过。Linux/cgroup/RSS/Long-run 验证仍未运行：本机没有可用 Linux 测试环境，103 宿主仅剩约 16 GiB 磁盘、低于 Rust 门禁的 22 GiB 前置，未在 CI 宿主上启动冷构建。

## 共享账本原子性（实施中）

现有 pending 与 committed-unconfirmed 各自 CAS，跨计数器申请会同时通过；commit 先减 pending 再加 committed 会短暂开放已承诺额度。确定性回归测试在恢复旧算法时均失败：commit 窗口误开放 905969664 bytes；交错的 pending/committed 申请同时获准。

修复采用一个 outstanding 原子总量作为唯一额度门槛，分类迁移不改总量；pending 取消/缩小退款，已提交字节仍等待物理观测。独立复核确认此变更范围内的生命周期与读写顺序正确。预留尚未实际分配却被视为已观测，是单独待修的生命周期问题；不能把本次原子修复当成完整保留保证。

## 已确认的后续约束

- probe：已由 `e33e58a` 修复「多批部分成功后整批回填」与「计划切换 abort flusher」；上限同时覆盖排队与 in-flight，追加任务丢失计入受控丢弃，状态见 `probeOutbox`。溢出时保留较旧观测、拒绝新观测。
- daily：已由 `bf82d3b` 改为按 wire cap 分块并只回填未持久化行；pending 有行数与字节上限，截断时保留较旧行并写 gap。核心 server 行与 domain 行分别限额，但 spool 完全拒绝写入时两者都会受控丢弃。
- metric：已由 `bf82d3b` 取消失败后 restore 全部聚合样本；未持久化请求进入 4,096 项 / 16 MiB pending 队列，按原始时段、item、version 重试。接收端忽略 `keepKeys`、ClickHouse 替换表不求和已核实；尚未引入远端 ACK baseline。
- aggregator：已由 `208be7f` 增加 per-aggregator 字节预算，entry、动态属性与 request sample 在分配前计费；flush 退款、restore 重新计费。top_ip 与 daily tracker 仍只有条目上限。
- Linux 验证尚未运行。本机测试不证明 Linux RSS、cgroup 压力、吞吐或长连接生存。
