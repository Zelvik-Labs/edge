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

所有本机 Rust 测试通过工作区 `scripts/local-rust.sh`、mbx、单编译槽运行。未清缓存。未发布、部署或修改 tag。

## 共享账本原子性（实施中）

现有 pending 与 committed-unconfirmed 各自 CAS，跨计数器申请会同时通过；commit 先减 pending 再加 committed 会短暂开放已承诺额度。确定性回归测试在恢复旧算法时均失败：commit 窗口误开放 905969664 bytes；交错的 pending/committed 申请同时获准。

修复采用一个 outstanding 原子总量作为唯一额度门槛，分类迁移不改总量；pending 取消/缩小退款，已提交字节仍等待物理观测。独立复核确认此变更范围内的生命周期与读写顺序正确。预留尚未实际分配却被视为已观测，是单独待修的生命周期问题；不能把本次原子修复当成完整保留保证。

## 已确认的后续约束

- probe：已由 `e33e58a` 修复「多批部分成功后整批回填」与「计划切换 abort flusher」；上限同时覆盖排队与 in-flight，追加任务丢失计入受控丢弃，状态见 `probeOutbox`。溢出时保留较旧观测、拒绝新观测。
- daily：累积 pending 可能超过单包上限后永久重试；核心 server 统计与可丢弃 domain 详情必须分别处理。
- metric：`keepKeys` 在当前接收端未使用；enqueue 前更新 values_cache 会抑制失败样本。当前发送 flush 增量，但同周期 hash 相同，ClickHouse 替换表不提供增量求和、生产顺序版本或跨节点隔离。先证明实际语义，再落实持久 baseline；不能把 delayed ACK 当成值存在的证据。
- aggregator：动态属性在入队前和 worker 中已有分配；flush 移交而非释放内存。预算必须由属性/容器/批次持有，不能 flush 就退款，也不能 restore 时重新申请导致丢弃已有数据。
- Linux 验证尚未运行。本机测试不证明 Linux RSS、cgroup 压力、吞吐或长连接生存。
