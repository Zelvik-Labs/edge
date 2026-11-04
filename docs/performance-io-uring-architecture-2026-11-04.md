# io_uring SPLICE 的下一步：先改变 I/O 路径

现有共享 io_uring SPLICE 在已测小包档位慢于 readiness；继续减少 io-wq 数量没有可靠收益。
本轮源码审计找到一项明确约束：Linux v6.8 的 `__io_splice_prep` 无条件设置
`REQ_F_FORCE_ASYNC`，不因 `SPLICE_F_NONBLOCK` 而免除工作队列调度。
Edge 实验实现已经为读、写两类 SPLICE 设置 NONBLOCK；它并没有显式设置用户 SQE 的 ASYNC 标志。
所以简单删除用户态异步标志或继续增加 ring 深度，不能消除这个内核路径本身的调度。

## 已确认

[上游原文件](https://github.com/torvalds/linux/blob/v6.8/io_uring/splice.c) 中，
`io_splice_prep` 调用共用准备函数，该函数验证 splice flags 后直接设置强制异步。
执行函数还对 `issue_flags & IO_URING_F_NONBLOCK` 做 WARN；这个 **io_uring issue flag**
与调用者传入的 **SPLICE_F_NONBLOCK** 不是同一组标志，不能混为一谈。
Edge 当前 `crates/zelvik-edge-io/src/uring.rs` 则经历 socket→pipe、pipe→socket 两个提交阶段，
另有 issuer 的请求接收、SQ/CQ 和完成通知。源码支持“有这些阶段”，不能单凭阅读给每阶段分配耗时比例。

[已完成的 Actions 41529553377](https://github.com/Zelvik-Labs/edge/actions/runs/41529553377)
使用 `6.8.0-1070-azure`，72 个组件场景和 16 项实验 I/O 测试通过；
那轮与 readiness 的全部 18 个配对都更慢，线程和 CPU 限额已校验。
下表引用[原始报告](performance-io-wq-bounded-2026-11-02.md)，不是这轮新增实测。

| 连接数 | 1 KiB 耗时变化 | 64 KiB | 1 MiB |
|---|---:|---:|---:|
| 1 | +163.02% | +71.54% | +12.13% |
| 16 | +64.15% | +25.85% | +8.53% |

## 下一项实现的边界

保留 readiness splice 作为真实节点对照。新的 io_uring 候选应优先试验 RECV/提供缓冲区与 SEND 路径，
分别验证少一次工作队列交接能否抵消用户态正文复制的成本，再考虑 multishot 接收和批量提交。
第一版需要同时保持半关闭、RST、取消后已接受字节统计、背压和读取期限，不能只测无限发送。
如果进一步引入 SEND_ZC，缓冲区寿命必须覆盖通知完成，不能在首次 CQE 到来时就释放；
小包复制和大包零复制应分开测量，而非假定零复制总是更快。

CPU、io-wq 线程数量与掩码继续沿用现有显式预算和读回验证。任何性能结论必须来自同机、同正文、
同连接数的完整往返对照，并同时记录 CPU、P99、线程及错误，不能只比较每秒 SQE 数量。
本轮尚未实现上述 RECV/SEND 候选，也未改变生产引擎选择。

## 设计限制与仍待确认

这里检查的是上游 v6.8 源码，不是 Ubuntu Azure 内核源包的逐字节审计；
不能宣称全部差距已经由强制异步解释，也没有内核调度轨迹证明各跳转的独立耗时。
结论仅约束当前 SPLICE 候选，不推广到所有 io_uring opcode、SQPOLL、长流或真实 NIC。

## 经审批的功能降级

无。本轮仅补充可复核的架构依据，未移除或默认启用实验路径。

[源码与校验信息](evidence/io-uring-architecture-2026-11-04/)保留上游原始 SPDX、原文件和 Edge 审计版本。
上游文件 SHA-256：`0bdd64d1d5f00fe5082d52606f0164a47ec369114b650c91aa64dbb46fb43a11`。
