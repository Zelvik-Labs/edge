# 持续性能优化轮次（2026-11-02）

范围按用户最新指令扩展至整个 Edge，包括 XDP/AF_XDP、socket I/O、io_uring、SNI 转发、HTTP/QUIC、缓存及统计。每轮记录问题、代码变化、测量条件、失败、限制及 GitHub Actions 的实际提交和结果。RKE 暂缓的指令继续有效。

## 基线归档与 CI

本轮起点与远端 main 一致：`01a2156882789f876f3c40c01bc6a0ee0bc5fa2b`。此前各阶段已实现的优化和证据将先提交到独立性能分支，使用现有 CI workflow 的 workflow_dispatch 检查公开仓库中的实际提交。默认分支和 release workflow 不在本次操作范围。

已完成的本地验证：411 项相关 Rust 测试通过、1 项既有测试忽略，Clippy lib/tests 通过；修复后 11 项 H1/H2/H3 功能场景通过。空最终正文警告在匹配诊断中从 10,897 条降为 0。来源见[重试缓冲修复](performance-h3-retry-buffer-2026-11-02.md)。

已有容量与失败全部保留：[协议和状态对比](performance-protocol-state-comparison-2026-11-02.md)、[H3 联合候选](performance-h3-stage2-2026-11-02.md)、[接收消融](performance-h3-receive-ablation-2026-11-02.md)。单包消融的 12 次超时仍未确认根因；本地测试和微基准均不能代替 GitHub Actions，也没有证明整体超过 Nginx/OpenResty。

## 下一轮检查重点

1. TCP/SNI 默认 splice 路径每方向创建 spawn_blocking 任务，检查并发扩展成本、半关闭、取消和 FD 生命周期；与 Tokio socket readiness 驱动对照。
2. AF_XDP 唤醒、批量收发、队列与副本：区分纯组件、netns/veth 功能验证和真实 NIC 数据，不将虚拟设备成绩当作硬件零复制容量。
3. 当前没有 io_uring 实现。先测本云环境实际系统调用能力，再建立与现有 socket/splice 相同校验的候选；旧文档中的暂缓结论由本次用户指令更新。
4. 保留证书、完整正文、缓存状态、连接数、回源数、超时、取消和资源边界检查；逐项优化并记录回退。

基线已推送到独立分支 `perf/full-stack-20261102`。GitHub Actions [41497645026](https://github.com/Zelvik-Labs/edge/actions/runs/41497645026) 对提交 `6f942440df52b7797fbb46cec86cef916681deef` 的两个 job 均成功：all-targets check / Clippy、Rust 单元与集成测试、vendored Pingora 检查与测试，并完成 XDP eBPF 构建。[job 与 step 记录](evidence/perf-continuous-2026-11-02/baseline-github-actions.json)。此结果仅属于该基线提交，后续源码改动需另行运行 CI。

## 第 1 轮：TCP/SNI 内核 splice

改为 socket readiness 驱动，取消每连接两个阻塞任务和六个 socket FD 副本，修复半关闭后响应丢失，并移除与实际实现不符的阻塞池准入上限；内存、FD 和压力保护保留。本地 77 项相关测试及 Root/I/O 全目标 Clippy 通过。三轮反向对照中，旧实现的线程池停滞和半关闭错误各复现 3 次，新实现均成功。16 条连接的线程增量 32→0，FD 增量 224→128。

本地 18 个组件场景全部成功，小包有收益，1 MiB 有一轮回退 +23.80%，全部保留。实现提交 `2df8b242b1b0d24f199e22592b6924e592f960a2` 的 [Actions 41508571673](https://github.com/Zelvik-Labs/edge/actions/runs/41508571673) 两个 job 成功，2,241 项测试通过、0 失败、4 项既有忽略；CI 的 18 个独立进程基准也成功。CI 三轮配对耗时中位数：1 KiB -37.92%、64 KiB -7.97%、1 MiB +1.43%，大包尚无明确收益。不是完整 Edge 或 Nginx/OpenResty 容量结论。新增可选 `splice_benchmark=true` 输入，保存原始样本及失败。完整实现、条件、限制与校验后的 CI 产物见[第 1 轮报告](performance-tcp-splice-2026-11-02.md)。

## 第 2 轮：大包诊断与协作预算消融

已对相同 885 次 1 MiB 往返做系统调用追踪，新引擎的 splice、epoll 与 futex 次数均减少，未发现 syscall 数膨胀。Tokio 1.53.1 的 `async_io` 已有协作预算，因此另测删除额外 `consume_budget` 的候选：功能和 I/O Clippy 通过，两组共 36 个测量场景成功，但未见稳定收益，大包在两组中的配对中位数分别回退 +2.44%、+15.10%。另发现同源重建后二进制与代码段发生变化，构建/调度/宿主波动仍待分离，不能把全部差异归因于那一行代码。该候选不采用，运行时代码已逐字节恢复到第 1 轮通过 CI 的实现；所有结果保留。细节见[第 2 轮报告](performance-splice-budget-ablation-2026-11-02.md)。后续继续推进 io_uring、AF_XDP 与完整协议/缓存路径，不能把本轮诊断当作这些工作已完成。

## 第 3 轮：共享 io_uring splice 候选

在 `perf/io-uring-20261102` 实验分支实现共享 ring、注册文件槽位、半关闭、精确取消、读取期限、有界准入与停止排空。10 项新增行为测试及原 I/O 5 项通过，默认/实验 feature 的全目标 Clippy 通过。使用同一二进制对 readiness/io_uring 做 1/16 条连接的 36 个本地场景，正文与计数均成功，但六个档位的耗时中位数均回退。Linux v6.18.57/v6.8 源码确认 SPLICE 强制异步。

实际提交 `b88f24b8de525f71941948e231cbfbda1ee635e1` 的 [Actions 41524209297](https://github.com/Zelvik-Labs/edge/actions/runs/41524209297) 两个 job 均成功，36 个 CI 组件场景全部通过、CPU steal 为 0%。配对耗时中位数：单连接 1 KiB / 64 KiB / 1 MiB 分别 +121.09% / +66.78% / +6.10%；16 连接分别 +37.11% / +15.09% / +9.80%。该内核上还观察到 8～175 个 io-wq 线程；下一轮限制内核工作队列进行消融，不能用未限额候选代表全部 io_uring 方案。候选继续默认关闭，不接入代理。第一次 CI 的准备阶段 HTTP403 与后续凭据修复也已归档。完整样本、提交校验、线程和限制见[第 3 轮报告](performance-io-uring-2026-11-02.md)。

## 第 4 轮：io-wq 限额与 XDP 内核探针

实验后端启动时显式注册内核工作线程 CPU 范围，并提供 `[2,2]` 上限候选。16 项 I/O 测试和默认/实验全目标 Clippy 通过；删除设置上限调用时，并发测试会因实际限额仍为内核默认而失败。基准增加逐线程 CPU、限额读回与数量检查，缺失或越界均失败。本地 36 个场景全部通过，候选各档均为 2 个 io-wq 线程；但性能方向不稳定，CPU steal 最高约 47.80%，全部离群值保留。

新增不附加接口的 BPF/AF_XDP 探针，本地确认 AF_XDP 返回 EAFNOSUPPORT，隔离 Docker 可加载最小 BPF 程序，容器清理成功。提交 `112815b891ea26e6d0e132e7a2d1487b9d6d8f9a` 的 [Actions 41529553377](https://github.com/Zelvik-Labs/edge/actions/runs/41529553377) 三个 job 全部成功，16 项 I/O 测试和 72 个计时场景通过，CPU steal 为 0%，所有观测线程都在 CPU 0/1 内。候选均为 2 个 io-wq 线程，默认数量对照最多观察到 394 个；但限额使小包耗时增加约 6%～16%，相比 readiness 的六组中位数仍慢约 9%～163%。不接入默认代理，后续优先 AF_XDP readiness 与完整协议路径。原生 6.8 内核探针确认 BPF 和 AF_XDP socket 可用。完整实现、全部快慢样本及限制见[第 4 轮报告](performance-io-wq-bounded-2026-11-02.md)。

## 第 5 轮：真实 AF_XDP ring 与唤醒行为

新增隔离 veth 的显式 SKB/copy 探针，核对 RX 突发、部分排空、fill starvation/恢复、TX 正文和完成队列，以及三次完整销毁后重绑。提交 `81f970c5580140fd89ab248a4ea2b031b551b1d5` 的 [Actions 41531098471](https://github.com/Zelvik-Labs/edge/actions/runs/41531098471) 三个 job 全部成功，含既有全仓门禁。原生 7.0 内核三轮各完成 65 RX / 1 TX 帧，主动耗尽 fill 时各有 1 个可观察丢弃，恢复和清理均成功。

RW+edge-triggered 初始 EPOLLOUT 后没有继续空闲唤醒，不能仅凭永久可写认定 Tokio 的自旋根因；部分排空 RX 后也没有第二个事件，后续 AsyncFd 必须持有 guard 到实际观察空 RX 后再清除。fill kick 为 0，因此未验证硬件 NEED_WAKEUP 或零复制。本轮建立功能验证，未改变节点唤醒后端，也不是性能提升或 Tokio 对照结论。所有原始结果和限制见[第 5 轮报告](performance-af-xdp-wake-2026-11-02.md)。

## 第 6 轮：Tokio 与 owned poll worker 对照

独立原生基准复用节点 worker 源码和上一轮 C ring fixture，对照 worker/channel、READABLE-only 和 RW Tokio 注册。保留 RX 空观察后的 guard 清除、驱动 poll/kick、完整正文、序号与 TX 完成检查；冻结并核对实际编译的基线，记录计时阶段唤醒次数以防忙路径绕过。8 项本地测试与 Clippy/release 构建通过；本地 11 项原生尝试均因 EAFNOSUPPORT 失败，已清理并保留空耗时。

提交 `7daf4b80b1971aa45e3d12086acbf2bf48f16de5` 的 [Actions 41542718770](https://github.com/Zelvik-Labs/edge/actions/runs/41542718770) 三个 job 全部成功。两个真实 XSK 功能控制和 9 个性能场景通过，90 万计时 RTT 样本复核成功，CPU steal 为 0%。READABLE 配对耗时中位数下降 67.56%，P99 下降 71.81%，组件吞吐约 3.08 倍；线程 2→1、FD 18→16。RW 方案耗时也下降 66.73%，不支持将旧问题仅归因于 RW 注册。worker 测量阶段有约 20 万次等待，直接 readiness 为 10 万次。

历史代码另有 `.ok()` 吞掉错误和收到 guard 后立即清除的问题，尚未复现旧节点整个 CPU 事件。本轮支持继续节点集成，但不外推为整个 Edge 提速 3 倍；默认节点仍保留 owned worker，硬件与完整协议/长稳对照仍待推进。细节和所有原始样本见[第 6 轮报告](performance-af-xdp-tokio-2026-11-02.md)。

## 第 7 轮：节点 readiness 实验接入与测试状态隔离（延续至 11-03）

将直接唤醒接入显式 `proxy-smoke/proxy-reload-smoke --wake readable`，默认服务继续使用 worker。拥有复制 FD，部分读取持有 guard，只有后续真实空 RX 才清除；队列错误继续走故障与 abort/RST。真实 UDP 错误测试发现 Tokio 1.53 的 `readable()` 过滤纯 ERROR 事件，候选改为同时注册和等待 READABLE|ERROR。保留 fill、CQ、驱动 poll/kick 与原生产配置。

全仓门禁也暴露了 fake-time shedding 测试与真实压力观察共用原子状态、H3 NAT 用例依赖全局压力滞回等问题。生产仍使用相同单例状态和完整压力组合，测试拥有独立 shedding 状态和明确的完整 QUIC 压力观测；保留所有原断言，并增加并发状态隔离、高压按 IP 限流、总路由上限用例。修复后本地完整库 1,300 通过、0 失败、2 项既有忽略，全目标 Clippy 通过；独立唤醒包 13 项通过。

前三次 CI 分别停止于过期 Cargo 目标名、预编译模式不必要的 Cargo 前置检查、共享测试状态导致的 8 项库测试失败；均归档，未算原生协议成功。源码最终提交 `4651bd39f576687240bbefe99bfee38927b96c3d` 的 [Actions 41553649436](https://github.com/Zelvik-Labs/edge/actions/runs/41553649436) 两个执行 job 均成功，原生 Linux 6.8 四轮 worker/readable 对照全部通过：IPv4/IPv6 八种协议路径、真实 generation reload、网络清理与前后二进制一致性均复核成功；8 次 H2 和 16 个 H3/QUIC 探针成功，所报队列错误与丢弃计数均为 0。没有整机吞吐提升或超过 Nginx/OpenResty 的新结论。完整过程与原始证据见[第 7 轮报告](performance-af-xdp-node-readiness-2026-11-02.md)。


## 第 8 轮：完整节点 UDP 复用对比（11-03）

新增 byte/sequence 校验 C 负载工具、独立回源、计时 gate 和 CPU/FD/线程/steal 观测。release Edge 两种 wake、官方固定版本 Nginx/OpenResty 在相同四 CPU 分工、64/1200 字节、1/16 流下做四轮轮换对照；源端口数证明真实复用，UDP 无重试。源码 `4b29028edb47e6a038392d066eee747e2f34d3f7` 的 [Actions 41561819976](https://github.com/Zelvik-Labs/edge/actions/runs/41561819976) 两个执行 job 成功，68/68 场景通过、578 万 RTT 独立复核，双向 XSK redirect 和流水线计数相符，所报错误/丢弃为 0。

READABLE 单流吞吐配对中位数 +14.81%/+13.51%，16 流却 -3.62%/-3.56%，默认 worker 不切换。四档仍落后于基线；基线 16 流客户端均超过 90% CPU，15 个单流基线样本短于 0.5 秒，不能视为最大容量。全部离群值保留，含单流大报文 P99 回退 +59.72%。仅为 IPv4 UDP 单队列 SKB/copy，不代表 H3/缓存或硬件性能。完整结果与已核对原始产物见[第 8 轮报告](performance-af-xdp-udp-reuse-2026-11-03.md)。后续优先实际 CPU/系统调用与重复解析归因，不将组件 3 倍直接外推整机。
