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

18 个独立进程组件基准场景全部成功：1 KiB、64 KiB 配对耗时变化的中位数分别为 -36.41%、-33.50%；1 MiB 有一轮回退 +23.80%，未排除，仍待 CI 复测和剖析。不是完整 Edge 或 Nginx/OpenResty 容量结论。新增可选的 `splice_benchmark=true` Actions 输入，保存原始样本及失败。完整实现、命令、环境、限制与证据见[第 1 轮报告](performance-tcp-splice-2026-11-02.md)。候选 CI 尚待运行。
