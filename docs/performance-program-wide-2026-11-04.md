# 全程序性能优化：本轮交付与验证边界

[最初五项工作](performance-five-priorities-2026-11-04.md)已完成对应实现和短时验证。
本页记录随后跨缓存、SNI/TCP、统计、XDP 与 io_uring 的推进，独立实验的百分比不能相加。
源码分支 `perf/program-wide-20261104`，未合入 main 或部署。

## 已修复与实测

| 工作 | 变化与证据 | 结论边界 |
|---|---|---|
| [缓存优化后 CPU 复核](performance-http-cpu-after-cache-2026-11-04.md) | 实际 ba474 二进制，18 条件、496 万次响应、43,074 样本、零错/丢样/steal；H1 HIT 请求过滤约 17%、头处理 15%、维度处理 8% | 包含式归因可重叠；下一轮定位正常路径，不将采样吞吐排名 |
| [实际 HTTP CPU 归因](performance-http-cpu-2026-11-04.md) | 18 个真实节点条件、328 万次响应、38,922 个 CPU 样本；定位缓存准入、头处理、分配与统计成本 | 采样流量不做容量排名，包含占比不相加 |
| [Range 缓存准入](performance-range-preflight-2026-11-04.md) | 无 Range 时跳过冗余完整对象预检查；192/192 对比通过、821 万次响应；HIT 吞吐配对 +3.95%～+10.08% | H1/H2 仍低于 Nginx/OpenResty，MISS 改善不稳定 |
| [已存响应头校验](performance-stored-headers-2026-11-04.md) | 31,981 组差分全等；典型 6 字段组件耗时约 −35%、分配 8→6；保留原解析器和安全检查 | 大于 32 字段使用旧路径；192 格节点约 0～1% 吞吐变化，部分 CPU 略退步；完整 CI 已通过 |
| 缓存上下文所有权 | `a79b20d` 将冗余 clone 改为 move，避免复制字符串和增加 Arc 引用；[CI 41715819291](https://github.com/Zelvik-Labs/edge/actions/runs/41715819291) 的 1361 项库测试、16 项二进制测试及集成/vendor/check/Clippy 通过 | 没有单独吞吐数字；[日志摘要](evidence/cache-context-2026-11-04/verification.json) |
| [SNI 解析](performance-sni-cursor-2026-11-04.md) | 不完整输入零分配；大分片跨记录读取、小正文栈缓冲；完整 CI 和 32 组原生节点通过 | 组件大分片约 −45%，仍需真实 SNI 端到端吞吐；局部退步保留 |
| [TCP/SNI 统计](performance-relay-copy-2026-11-04.md) | 取消与部分写入按实际接受字节结算，减少一个方向的一次计数器分配；组件与完整 CI 通过 | 64 字节短写组件约 +3.81%，不隐藏该代价 |
| [共享 QUIC 定时维护](performance-quic-maintenance-2026-11-04.md) | 定时清理复用局部路由压力，避免全局健康时仍按普通 TTL 保留过期元数据；1363 库测试及 32 组节点通过 | 尚未证明解释了此前共享长批次超时，无新吞吐数字 |
| [AF_XDP 自动绑核](performance-afxdp-affinity-2026-11-04.md) | 按继承的实际 CPU ID 选队列 CPU，修复稀疏 taskset 的错误；原生控制和累计 32 组完整节点通过 | 旧 UDP 对比显式绑核，因此该修复不能解释原吞吐差距 |

## 仍未修复

[Quinn GSO 对照](performance-quinn-gso-2026-11-04.md)稳定复现原生 Quinn 的新连接错误：
262,144 次 fresh 尝试中 77 次失败。auto 50 次、off 27 次，关闭 GSO 未消除故障。
复用控制中关闭 GSO 使吞吐下降约 13%～21%，继续保留 auto 默认值。
[有界协议诊断](performance-quinn-diagnostic-2026-11-04.md)已抓到 10 次 OpenSSL 本地 `KEY_UPDATE_ERROR`，
原因均为 `RX key update again too soon`，第二轮 29 个错误又记录到相同早期 key phase 时序。
[同版客户端互操作](performance-quinn-interop-2026-11-04.md)中，OpenSSL 16/32768 错误、GnuTLS/ngtcp2 32768 次通过，
四项错误 CA 验证均拒绝；同一 OpenSSL 的默认混合组 14/32768 错误、X25519 8/32768，混合组不是必要条件。这约束故障范围但不代表已修复。[收包节奏控制](performance-quinn-receive-spacing-2026-11-04.md)使用同一实际二进制与有界 relay 进一步检验接收分层假设，尚在运行。
共享路径两个长批次超时，部分输出和缺失的进程摘要均保留，不当成完整性能结果。

首轮 Range 对比因源站启动时序失败，已保留 161 次 curl 56 与 7 次 curl 52 的全部记录；
修复的是计时前源站就绪检查。复测重新执行完整矩阵，未重试或抹掉计时阶段的错误。

## 设计限制与下一阶段

[io_uring 源码审计](performance-io-uring-architecture-2026-11-04.md)确认 Linux v6.8 的 splice 请求被强制异步处理，
不能因设置 NONBLOCK 就视为 readiness 等价快路径。此前组件实验全部配对较慢，继续保留 readiness 默认。
后续 RECV/SEND 与 provided-buffer 方案需要单独实现、取消/半关闭验证及同机对照，当前尚未实现。

AF_XDP 的队列线程、运行时交接与拷贝仍有分析空间；目前没有真实 NIC zero-copy 收益，
云 veth copy-mode 的结果不能外推到硬件。统计内置聚合仍有成本，本轮未通过停止统计提升成绩。

[统计维度所有权](performance-metrics-move-2026-11-04.md)减少 worker 的冗余 Arc clone/drop，完整 CI 与隔离 192 格对照在运行；当前不预报吞吐收益。
[共享可写等待](performance-shared-quic-writable-2026-11-04.md)已修复队列满仍立即 Ready 及 socket 单等待者问题，1366 库测试、集成/vendor/lint 与 32 组节点验证全部通过。
[UDP 出口复制候选](performance-udp-egress-2026-11-04.md)已测两种策略：满队列少一次分配，但普通发送分别有 5%～7% 与 1.7%～2.2% 代价，保留实验、不进入生产。
[共享内核 GSO 候选](performance-shared-quic-tx-2026-11-04.md)通过 IPv4/IPv6 真实分段、GRO 关闭与硬错误控制，尚未接入生产或测量节点收益。

响应头真实节点收益已核验，组件快约 35% 未转化为同量级节点收益。仍需真实 SNI 端到端吞吐，以及 24/72 小时稳定性和受控弱网。
当前没有全面超过 Nginx/OpenResty 的结论。

## 经审批的功能降级

无。缓存安全校验、协议、配置兼容、统计与故障可观测性保留；未部署、未操作 RKE。
