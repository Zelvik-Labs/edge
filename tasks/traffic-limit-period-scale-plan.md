# 配额周期下沉与超大规模性能计划

日期：2026-10-10。范围：Edge（Rust/Pingora）为主 + control-plane 下发字段。基线：edge `main`；control-plane `67da958`。

## 0. 结论（已决策，执行者不要再选型）

配额周期（套餐流量重置周期）由 **Edge 本地窗口**承担，控制面只下发参数（**方案 A**）。

理由（性能与规模）：
- 窗口判定是节点本地 O(1)；窗口边界只在跨界时重算一次，请求路径仍是一次比较。
- 天/季/年粒度严格正确（方案 B「控制面折算成 monthlySize」有损）。
- 不引入控制面配置推送抖动：方案 B 的推送量 ∝ 节点数 × 站点数，周期滚动要重推配置、节点重载 server 配置、缓存与连接元数据失效。
- 断连/控制面不可达时周期仍能正确滚动（节点自持窗口）。

## 1. 现状（已核实）

- 控制面 `internal/gcloud/services_contract.go:63` 把 `included_traffic_gib` 硬编码为 `trafficLimit.monthlySize`（Edge 侧字段名即 monthly）。
- Edge HTTP 判定在 `src/proxy.rs`（`enforce_traffic_limit` / `has_valid_traffic_limit`），UDP 判定在 `src/udp_proxy.rs:1770`。
- Edge 已有规模能力：mimalloc（`src/main.rs`）、`memory_governor`（按 CPU + 内存推导 pingora worker 线程 / keepalive 池 / accept worker / UDP demux 线程数）、XDP/AF_XDP（`src/proxy.rs`、`src/udp_proxy.rs`、`src/bin/bench-defense.rs`、`src/bin/xdp-verify.rs`）、Mace bucket（缓存元数据 / WAF token / metrics）。

## 2. 未核实项（执行者第一步：先读，再动手）

必须先定位并记录到本文件末尾的「实测记录」小节：

1. `struct TrafficLimit` 的真实定义位置与全部字段；
2. 窗口计数的载体：原子计数 or Mace bucket；
3. 现有窗口语义：按月固定 or 按窗口起点滚动；
4. `enforce_traffic_limit` 的返回语义（bool → notice body）与调用点。

这决定改动是「只加两个字段」还是「顺带把窗口逻辑重写」。**不要跳过。**

## 3. 契约变更（先改契约，再改实现）

`ServerConfig.traffic_limit`：

```text
is_on: bool
window_limit_bytes: u64                      # 替代 monthlySize
period_unit: "day"|"month"|"quarter"|"year"  # 缺省 month
period_interval: u32                         # N，缺省 1
notice_page_body: string                     # 保持现状
```

- 字段缺失/缺省 → 退化为 `month × 1`，与现状等价（零破坏）。
- 控制面 `services_contract.go` 改为下发 `period_*`；`included_traffic_gib` 只作为 `window_limit_bytes` 的来源。
- 同步契约文档：`docs/docs/contracts/`、`docs/docs/13-billing-and-settlement.md`（跨仓唯一事实源，先改这里）。

## 4. Edge 实施步骤

1. 窗口起点 `align(now, period)`：月/季/年按节点本地时区对齐到自然边界，day 对齐自然日；结果缓存 `(window_start, window_end)`。
2. 跨界：**只在跨界时重算**；计数翻转用一次 CAS（`compare_exchange`），失败方重读；禁止锁。
3. 热路径：判定 = `used_bytes >= window_limit_bytes` 一次比较；不得在请求路径做日历运算、分配或加锁。
4. UDP 路径（`udp_proxy.rs`）复用同一窗口结构体，避免两份实现漂移。
5. 指标：暴露 `window_start`、`used_bytes`、`limit_bytes`、`period`；不加每请求直写，走本地聚合 + 批量上报。

## 5. 超大规模性能要求（逐项含验收口径）

1. **热路径零锁零分配**：`bench-defense` 证明限速判定 p99 无回归。
2. **accept 亲和**：每 worker 独立 SO_REUSEPORT 监听 + CPU pin；验收：`perf` 无 accept 跨核迁移、无惊群。
3. **XDP/AF_XDP 前置丢弃**：L4 攻击/黑名单在 XDP 层丢弃；验收：`xdp-verify` + `bench-defense` 改动前后对比（丢弃路径吞吐、CPU）。
4. **缓存元数据分片**：Mace bucket 分片数随核数扩展；验收：多核并发下 bucket 争用（`perf` 等锁/自旋）显著下降。
5. **回源连接复用**：keepalive 池按线程预算（已有）；验收：回源新建连接数 / 请求数 比值在高并发下保持低位。
6. **遥测批量化**：本地聚合 + 批量异步上报；验收：QPS 翻倍时上报 CPU 占比不线性增长。

## 6. 验证矩阵

- `cargo build --release`、`cargo test`、`cargo clippy -- -D warnings`。
- `bench-defense` 与 `xdp-verify` 基线：改动前后记录 p50/p95/p99、RPS、CPU、RSS。
- 限速正确性用例：跨月/跨季/跨年边界、`period_interval=N>1`、时区、时钟回拨、`window_limit_bytes=0`、字段缺失退化为 monthly。
- 断连场景：控制面不可达时窗口仍正确滚动。
- 规模用例：多站点 × 高 QPS 下的计数争用与内存增长。

## 7. 边界与风险

- 不在本计划内：L4（TCP/UDP）速率整形（当前只有用量配额判定）；跨节点共享额度（需中心化令牌桶）。
- 时钟回拨：窗口按 `now` 单调推进，回拨不回退窗口（避免重复放量），记告警。
- 保持 `notice_page_body` 超量提示页兼容。

## 8. 交付物

- Edge：`traffic_limit` 窗口实现 + 单元/集成测试 + 基准数据。
- control-plane：`services_contract.go` 下发字段改造 + 契约文档。
- 本文件的「实测记录」小节（执行者填写第 2 节的四项与全部基准数字）。

## 实测记录（执行者填写）

- `struct TrafficLimit` 定义：
- 计数载体：
- 现有窗口语义：
- `enforce_traffic_limit` 返回语义与调用点：
- 改动前基线（bench-defense / xdp-verify）：
- 改动后对比：
