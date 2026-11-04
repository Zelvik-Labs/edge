# 本轮架构定位与性能证据收尾

本轮新增 Hyper 基础控制与同 ELF 缓冲因子实验，补齐 syscall/CPU/futex 原始归档复核。没有修改生产 HTTP 核心、上游库或部署，也没有把组件结果写成节点收益。

## 已完成与可定位差异

- [Hyper 五产品对照](performance-bare-hyper-2026-11-04.md)：75 格、4,182,110 有效响应、零错误，50 个 wrong-CA 控制；裸 Hyper H1-TLS 中位 RPS 略高于 nginx/OpenResty，H1 仍低、H2 介于两者。不能用不含缓存/WAF/统计的控制替代完整 Edge 的结论。
- [同一 ELF 缓冲实验](performance-pingora-buffering-2026-11-04.md)：48 格、4,013,938 有效响应、零错误，32 个 wrong-CA 控制；仅加 1460B 写缓冲，H1/H1-TLS 四轮配对吞吐中位 +10.78/+10.60%，CPU/响应 -8.54/-8.56%。H2 变化小且有负向轮次，64KiB 读缓冲无稳定收益。
- [futex 调用者](performance-futex-stacks-2026-11-04.md)：在两个客户端已有完成记录后采集的 1,246 样本中，1,244 来自 Mace 后台等待、2 来自内存回收，操作均为 WAIT_BITSET_PRIVATE。等待次数不能当 CPU，更不能把 counts-only 的 futex/响应比值解释成每请求统计唤醒。
- [syscall 复测](performance-framework-syscalls-2026-11-04.md)16 格/328,115 成功；[框架 CPU](performance-framework-profile-2026-11-04.md)12 格/1,613,653 成功；[交叉产品 CPU](performance-cross-product-cpu-2026-11-04.md)24 格/23,958,632 成功，均已重新核对实际输入及完整记录。采样/ptrace 下 RPS 不进入容量排名。
- [最终归档复核 Actions](https://github.com/Zelvik-Labs/edge/actions/runs/41816776890)：三项报告边界测试与七组全部通过。[冻结数据与重放说明](evidence/performance-review-2026-11-04/README.md)保留原 ZIP、源码身份、每格记录、失败和负向变化。

两项新容量实验的实际 CPU 分配均为 proxy=[0]、client=[1,2]、origin=[3]，即 1/2/1；各自内部一致。不同 runner 的绝对 RPS 仍不混比。累计旧改动的真实 Edge 192 格此前没有稳定显著收益，结论保持；不把本轮基础控制的 11% 冒算为 Edge 提升。

## 下一步的具体生产候选

优先读 0/写 1460B，保留原 L4Stream 具体类型。现有私有缓冲设置 API 无法供 Edge 的自有 accept 使用，普通包装会遮蔽明文 sendfile，整体改监听则牵动 SNI/PROXY/动态 worker。已准备[新流构造器的最小提案](performance-buffer-constructor-proposal-2026-11-04.md)：只在创建时设容量、项目入口使用、增加 1460B 内存估算并验证 flush/sendfile/慢读者/升级/取消，随后构建真实前后 ELF 与全面节点对照。该提案触及 AGENTS 明确禁止的 upstream 路径，尚未应用，须取得该处的定向例外。

第二优先继续定位业务头处理、分配与后台空闲工作；Hyper 可作为核心重构的执行成本参考，尚未决定替换整个 Pingora。H3、XDP/AF_XDP、SNI 和 io_uring 仍按已有证据推进，不能把本轮 H1/H2 数值外推到它们。

## 仍存在、限制与降级

整体 Edge 与 nginx/OpenResty 的性能差距仍存在，功能等价架构改造及真实节点提升尚未完成。短云窗口不代表 24/72 小时稳定性，没有真实 NIC/零复制新结论。历史所有测量/构建/校验失败均保留；本轮复核器自身两次错误也明确记录，后续成功不覆盖它们。

云管理面一度显示 connected，但命令工具仍返回 No usable execution environment；没有把配置状态当作可执行工作区已经恢复，也未认定离线由磁盘、配额或 VPN 导致。独立 Actions 能继续，原源码已在性能分支；未处理 RKE、未合入 main。经审批的功能降级：无。
