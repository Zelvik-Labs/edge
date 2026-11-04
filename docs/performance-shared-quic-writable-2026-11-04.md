# 共享 QUIC 的可写等待修复

共享 H3 的 AF_XDP 发送队列满时，try_send 返回 WouldBlock，但旧 poller 无条件报告 Ready，不能等待容量恢复。
内核 socket 路径直接使用 poll_send_ready，只保存一个发送等待者；Quinn 的 UdpPoller 合同允许同一 socket 上任意多个独立 poller。
这两处都属于背压与调度问题，尚不能据此解释之前的新连接超时或宣称节点吞吐提高。

修复为每个 poller 保留独立、可取消的等待 future：内核使用 Tokio writable；channel 等待 reserve 后立即释放建议性容量，
实际发送继续通过 try_send 检查，其他连接抢占容量时仍返回 WouldBlock。队列关闭返回 BrokenPipe。
成功与错误均清除已完成的 future，符合 Quinn poller 可反复复用的合同。
每个 poller 的 Mutex 只提供 future 所需的 Sync 边界，独占 poll 使用 get_mut，不新增共享锁竞争。
只有进入可写等待时才创建 future，不改变正常 try_send 路径、队列容量或数据包所有权。

新增三项回归覆盖：满队列多次 Pending 且不自行唤醒、排空后恢复与重复关闭错误；两个独立等待者的容量转移及取消；
current-thread reactor 中真实初始 socket 就绪事件对 IPv4/IPv6 两个等待者的唤醒及正文/来源验证。
socket 控制在新注册后首次 reactor 轮询前观察真实 WouldBlock，不制造不符合 Tokio 契约的错误。
它没有覆盖内核实际发送缓冲区耗尽，也不是压力吞吐测试。

本地仅做格式和差异检查，完整 Rust CI 与 32 组隔离节点验证交给 GitHub Actions，结果待核验。

仍待解决：共享路径逐段发送不能直接声明原生 GSO；内核 TX 候选尚未接入生产；早期 key-update 客户端互通错误仍待定位。
未修改上游或 vendor，未降低校验和既有防护。经审批的功能降级：无。
