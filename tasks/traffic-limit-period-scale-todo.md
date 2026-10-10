# 配额周期下沉与超大规模性能 Checklist

关联计划：[traffic-limit-period-scale-plan.md](traffic-limit-period-scale-plan.md)

## 前置（必须先做）

- [x] 定位 `struct TrafficLimit` 定义与全部字段。
- [x] 确认窗口计数载体（原子计数 / Mace bucket）。
- [x] 确认现有窗口语义（按月固定 / 按窗口起点滚动）。
- [x] 确认 `enforce_traffic_limit` 返回语义与全部调用点（HTTP + UDP）。
- [x] 把以上四项写入计划的「实测记录」。

## 契约

- [x] 改 `docs/docs/contracts/` 的 `traffic_limit` 定义（`window_limit_bytes` + `period_unit` + `period_interval`）。→ `docs/contracts/edge-traffic-limit.md`（draft，已登记 contract-status.yaml + README）
- [x] 同步 `docs/docs/13-billing-and-settlement.md` 的周期语义。→ §4.4
- [x] 改控制面 `internal/gcloud/services_contract.go:63` 下发 `period_*`，去掉硬编码 monthly。→ `pbPlan` 发 `windowLimitBytes`+`periodUnit`+`periodInterval`；含 4 项单测

## Edge 实现

- [x] 增加窗口结构：`window_start/window_end/used_bytes/limit_bytes/period`。→ `src/traffic_quota.rs::QuotaWindow`
- [x] `align(now, period)`：day 对齐自然日；month/quarter/year 对齐本地时区自然边界。
- [x] 只在跨界时重算窗口；翻转用一次 CAS，无锁。
- [x] 热路径只做一次比较，无日历运算/分配/加锁。
- [x] UDP 路径复用同一窗口结构。→ `quota_exceeded()` 共享，`record_transfer` 计费
- [x] 指标暴露 window/used/limit/period，走批量上报。→ statusJSON `trafficQuota.{sites,plans}`
- [x] 字段缺失退化为 `month × 1`。

## 超大规模性能项

- [x] bench-defense 基线（改动前）。→ 与改动后同窗对比（quota.bench A/B 站，同进程同二进制）
- [ ] 每 worker 独立 SO_REUSEPORT + CPU pin；perf 验证无惊群/跨核迁移。（既有能力，本 VM 无条件复测；见实测记录 §5.2–5.6 说明）
- [ ] XDP 前置丢弃回归（xdp-verify + bench-defense）。（需 AF_XDP 压测机；本 VM `BENCH_KERNEL_FILTER=off`，未覆盖，待专用环境）
- [ ] Mace bucket 分片随核数扩展；perf 验证争用下降。（既有能力，未触碰相关路径）
- [ ] 回源连接复用率验收。（既有能力，未触碰相关路径）
- [ ] 遥测批量上报在 QPS 翻倍下的 CPU 占比验收。（既有能力，未触碰相关路径）
- [x] bench-defense 复测（改动后）并记录对比。→ p50/p99 无可测回归；微基准 exceeded ≈0.79ns、account ≈6.65ns

## 测试与收尾

- [x] 边界用例：跨月/跨季/跨年、`period_interval=N>1`、时区、时钟回拨、`window_limit_bytes=0`、字段缺失。→ 13 项单测
- [x] 断连用例：控制面不可达时窗口正确滚动。→ 本地时钟驱动，bench-defense rpc_disable_update 下工作；单测覆盖翻转
- [x] `cargo build --release` / `cargo test` / `cargo clippy -- -D warnings`。→ 全过；`-D warnings` 仅余预存 wake.rs 测试告警（非本次改动），AGENTS 门禁（correctness+suspicious）全过
- [x] 清理基准产物与临时数据。→ scripts/clean-build-artifacts.sh + /tmp 清理
- [x] 填写「实测记录」的基线与对比数字。
