# 原生 H3 接收上限与最终功能验收（2026-11-04，香港时间）

实验性 tokio-quiche 后端已修正接收队列的预算假设。原先按 `2048 × 1500 B`
估算队列，但 tokio-quiche 0.20.0 在 Linux 上使用 64 KiB 的 `recvmsg` 缓冲区，
再按实际接收长度复制成入队 Vec；自动启用 GRO 后一次接收还可能包含多个 UDP 包。
QUIC 广告的 1350 B 接收参数不能约束解析前的入队分配，原 4 MiB/连接预留不成立。

## 修正

`ed01addd08abcb5718f767f13704b7d19692ec55` 在 Edge 自有适配层实现
`ingress::bounded_listener`，未修改上游或 vendor：

- 关闭接收 GRO，安装并锁定 Linux socket filter，只接收完整的、不超过 1500 B 的 UDP 正文。
  超限包整包拒绝；不截断后继续当作有效数据。IPv4、IPv6 使用同一规则。
- 保留发送 GSO 和 UDP 丢包计数能力。通过公开 `listen_with_capabilities` 进入库，避免默认入口重新启用 GRO。
- 有界清空 bind 到过滤器安装之间已排队的包，并在启动日志记录数量。
  初始化持续被流量占据、socket option 失败或非 Linux 平台均显式报错。
- 每个已准入连接预留 5 MiB，监听前为 32 个初始连接 backlog 加一个拒绝槽预留 165 MiB。
  这是覆盖队列及协议缓冲区的逻辑预算，不是实测 RSS，也不代表所有 QUIC 元数据的精确分配账本。
  普通连接准入预算仍另外计入；所有缓冲区克隆结束后才释放对应所有权。

这是实验后端的明确 socket 配置，仍未设为生产默认。接收 GRO 关闭有性能代价，
须在包含本修正的完整 CDN 矩阵中测量。修正前的原生矩阵不能冒充修正后的结果。

## 已核验的功能证据

| 验证 | 结果 | 原始工作流 |
|---|---|---|
| Linux 接收上限 | IPv4/IPv6 各接收 1/1200/1350/1500 B 完整包；拒绝 1501/65000 B；2700 B GSO 分成两个完整 1350 B 包 | [41672874554](https://github.com/Zelvik-Labs/edge/actions/runs/41672874554) |
| 适配层真实 H3 | 8 项全部通过，另加 10 轮 16 流并发复测；受控丢包实际为服务端/客户端 48/46 包 | 同上 |
| 最终依赖 FIN 因果对照 | 同一适配层与精确包顺序：旧依赖 EOF 断言失败，修复依赖通过；基线依赖身份集合未变化 | [41674107868](https://github.com/Zelvik-Labs/edge/actions/runs/41674107868) |
| 启用 h3-quiche 的完整节点 | check、Clippy、1343 个库测试及 16 个二进制用例通过；4 个性能探针显式 ignored；这些产物日志无 Rust warning | [41672874652](https://github.com/Zelvik-Labs/edge/actions/runs/41672874652) |
| 默认构建回归 | 1342 个库测试、16 个二进制用例及集成/vendor/check/Clippy 通过；32 种原生 AF_XDP 组合均通过且网络资源清理完整 | [41666609860](https://github.com/Zelvik-Labs/edge/actions/runs/41666609860) |

默认构建保留了原有 Cargo manifest 关于 `Zelvik-Edge` 非 kebab-case 二进制命名的提示，
不能把全部默认构建日志描述成零警告。前一轮默认 CI 的并发准入观察器曾在归还 permit 后才
减去观察计数，产生假峰值；`afd94f7` 调整测试 guard 的析构顺序，硬限额断言与生产准入逻辑保持原义。

最终 FIN 对照的第一次运行 [41672874659](https://github.com/Zelvik-Labs/edge/actions/runs/41672874659)
因离线 Cargo 缓存缺少 `boring` 的 sparse-index 条目，在测试开始前失败。
修复为允许正常注册表读取，仍断言基线所有包身份未变化；没有把该次失败算成旧依赖复现。

原始 ZIP、GitHub SHA-256、逐文件摘要及失败产物均保存在
[验收证据目录](evidence/h3-native-validation-2026-11-04)。不含测试私钥。仓库根目录可复核：

```bash
python3 docs/evidence/h3-native-validation-2026-11-04/verify.py
```

已修复：接收队列预算假设、已复现的 FIN 提前回收问题、控制实验的离线索引依赖。
仍待验证：长期运行、连接规模增长后的真实 RSS/预算偏差、公网 RTT/PTO 丢包组合。
设计限制：原生实验仍要求地址验证 Off、独占 UDP 端口且不使用 XDP；GRO 关闭，非 Linux 显式不支持。
经审批的功能降级：无；未用关闭生产功能或放宽协议测试换取通过。
