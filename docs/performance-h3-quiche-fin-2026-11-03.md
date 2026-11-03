# tokio-quiche 偶发结束信号丢失：证据与修复（2026-11-03，香港时间）

真实 HTTP3 测试曾偶发停在客户端 `recv_trailers()`：128 KiB 正文已经完整返回，
连接仍有充足 RX/TX 预算，最终等待到空闲超时。单次通过不能证明已修复，
因此保留了全部失败并增加 10 次重复执行和 qlog。

## 证据链

修复前 [e193f11 工作流](https://github.com/Zelvik-Labs/edge/actions/runs/41662643999)
的主测试通过，但重复第 8、10 次失败。逐包记录显示：

| fixture | 流 ID | 已发送流字节末尾 | 是否发出 FIN |
|---|---:|---:|---|
| edge-quiche-test-2959-0 | 12 | 131,091 | 否 |
| edge-quiche-test-2984-0 | 24 | 131,091 | 否 |

流字节包含 H3 帧及响应头，不等于正文长度。两次诊断均 `lost=0`、`retrans=0`；
完整正文已由客户端断言验证。其他 9 组 16 流连接都发出了 FIN。

quiche 0.30.0 的发送缓冲区判断“已完成”时只比较已 ACK 的字节范围和最终偏移，
没有单独要求 FIN 被 ACK。正文 ACK 与延后排队的空 FIN 竞争时，流可能提前被回收。
对应上游 [issue #2525](https://github.com/cloudflare/quiche/issues/2525) 与
[已合并修复 #2665](https://github.com/cloudflare/quiche/pull/2665)。

已将实验依赖固定到不可变提交
`af8788e03413ebb3cf81123fc4f1aacea44dfc40`，它增加 FIN ACK 状态，保留正确的重传/回收条件。
未修改任何 vendored/upstream 源码。`octets` 也必须固定到同源提交，否则发布版
tokio-quiche 与 git quiche 使用两个不同身份的同版本类型，导致编译失败；两份失败产物均保留。

修复后 [2ec6fff 工作流](https://github.com/Zelvik-Labs/edge/actions/runs/41664748391)
通过原有全部 5 个真实网络测试及 10 次重复压力测试。
独立重扫 qlog：**11 组 × 16 个响应流均发出 FIN，未再观察到上述缺失**。
没有放宽正文、trailers、证书、超时或失败判定。

## 验证范围

这一证据支持修复已复现的无丢包 FIN 回收问题，不证明所有 QUIC 丢包/重传路径均无缺陷。
上游另有 [issue #2800](https://github.com/cloudflare/quiche/issues/2800) 涉及 FIN 与 PTO/重传排序。
后续 [a04c47d 工作流](https://github.com/Zelvik-Labs/edge/actions/runs/41665849933)
通过全部 7 个网络测试和 10 次重复执行，包含“正文全部 ACK 后发送 FIN”和双向每 37 包丢 1 包。
丢包 fixture 实际丢弃服务端/客户端方向 49/46 个数据报，16 个并发流的 128 KiB 正文、重复响应头和双向 trailers 全部通过。
这仍不等于覆盖上游 #2800 的全部 PTO/重传排序。
[完整节点 feature CI](https://github.com/Zelvik-Labs/edge/actions/runs/41664748909)
也通过 check、Clippy 和 lib/bins 测试；日志未发现 Rust warning。
新增测试的第一次编译失败来自测试直接把 Bytes 传给需要 H3 WriteBuf 的 trait，
已改用公开 H3 客户端/服务端帧路径，失败原件仍保留。

实验使用 tokio-quiche 0.20.0 + 上述 quiche 修复 + Hyperium h3 0.0.8。
0.20.1 的 libc 上界与 Edge 当前依赖冲突；0-RTT 保持关闭。
这次结果不构成吞吐提升证明，也不将实验后端设为默认。

[原始 ZIP、摘要和独立复核脚本](evidence/h3-quiche-fin-2026-11-03) 已纳入仓库：

```bash
python3 docs/evidence/h3-quiche-fin-2026-11-03/verify.py
```

已解决：已复现的无丢包 FIN 丢失及依赖身份不一致。
本阶段之后已完成[完整 CDN 缓存基线](performance-h3-native-capacity-matrix-2026-11-04.md)、
[受控双向丢包及最终 FIN 对照](performance-h3-ingress-2026-11-04.md)与
[262,144 次持续连接](performance-h3-native-sustained-2026-11-04.md)。
长期运行与全部生产策略覆盖仍未完成。
设计限制：实验后端暂不覆盖共享 UDP/XDP/自适应 Retry。
经审批的功能降级：无。

## 因果对照的补充记录

[首次控制实验](https://github.com/Zelvik-Labs/edge/actions/runs/41667192163) 中，“所有 DATA 已 ACK 再排队 FIN”在旧版和修复版都通过。
因此该测试只覆盖一个合法结束顺序，不能独立复现此次竞争；工作流因预期旧版失败未出现而正确失败，原件保留。
实际缺陷需要“FIN 已排队但尚未发送，此时最后的 DATA ACK 到达”。新增逐包驱动的独立 QUIC 测试专门固定这一顺序，[因果对照工作流 41669006784](https://github.com/Zelvik-Labs/edge/actions/runs/41669006784) 已通过：
相同适配层和测试代码，旧依赖在最后的 EOF 断言返回 `Err(Done)`，修复依赖返回 `Ok((0, true))`。
TLS 握手、完整 4 KiB 正文、ACK 可用性和精确先后顺序均通过前置断言；没有通过等待超时制造差异。
当前 [全部适配层验证](https://github.com/Zelvik-Labs/edge/actions/runs/41669006839) 为 8 项测试及 10 次重复全部通过，
双向实际丢包仍为 49/46 个。原始旧版失败、修复版成功及首次未复现对照全部纳入同一证据目录。
