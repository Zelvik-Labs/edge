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

GitHub Actions 的提交、run URL、结论及后续每轮数据在实际执行后追加。当前没有将尚未触发的 CI 记为通过。
