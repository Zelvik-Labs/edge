# 配额周期下沉与超大规模性能 Checklist

关联计划：[traffic-limit-period-scale-plan.md](traffic-limit-period-scale-plan.md)

## 前置（必须先做）

- [ ] 定位 `struct TrafficLimit` 定义与全部字段。
- [ ] 确认窗口计数载体（原子计数 / Mace bucket）。
- [ ] 确认现有窗口语义（按月固定 / 按窗口起点滚动）。
- [ ] 确认 `enforce_traffic_limit` 返回语义与全部调用点（HTTP + UDP）。
- [ ] 把以上四项写入计划的「实测记录」。

## 契约

- [ ] 改 `docs/docs/contracts/` 的 `traffic_limit` 定义（`window_limit_bytes` + `period_unit` + `period_interval`）。
- [ ] 同步 `docs/docs/13-billing-and-settlement.md` 的周期语义。
- [ ] 改控制面 `internal/gcloud/services_contract.go:63` 下发 `period_*`，去掉硬编码 monthly。

## Edge 实现

- [ ] 增加窗口结构：`window_start/window_end/used_bytes/limit_bytes/period`。
- [ ] `align(now, period)`：day 对齐自然日；month/quarter/year 对齐本地时区自然边界。
- [ ] 只在跨界时重算窗口；翻转用一次 CAS，无锁。
- [ ] 热路径只做一次比较，无日历运算/分配/加锁。
- [ ] UDP 路径复用同一窗口结构。
- [ ] 指标暴露 window/used/limit/period，走批量上报。
- [ ] 字段缺失退化为 `month × 1`。

## 超大规模性能项

- [ ] bench-defense 基线（改动前）。
- [ ] 每 worker 独立 SO_REUSEPORT + CPU pin；perf 验证无惊群/跨核迁移。
- [ ] XDP 前置丢弃回归（xdp-verify + bench-defense）。
- [ ] Mace bucket 分片随核数扩展；perf 验证争用下降。
- [ ] 回源连接复用率验收。
- [ ] 遥测批量上报在 QPS 翻倍下的 CPU 占比验收。
- [ ] bench-defense 复测（改动后）并记录对比。

## 测试与收尾

- [ ] 边界用例：跨月/跨季/跨年、`period_interval=N>1`、时区、时钟回拨、`window_limit_bytes=0`、字段缺失。
- [ ] 断连用例：控制面不可达时窗口正确滚动。
- [ ] `cargo build --release` / `cargo test` / `cargo clippy -- -D warnings`。
- [ ] 清理基准产物与临时数据。
- [ ] 填写「实测记录」的基线与对比数字。
