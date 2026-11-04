# 共享 QUIC 的内核 TX 候选

本轮是独立 Linux 组件候选，尚未接入生产，也没有吞吐收益结论。
现有 shared socket 继承 Quinn 的发送聚合上限 1。其分段循环逐包发送，若中途遇到背压，
可能先发出一部分再返回错误，因此不能单独提高上限来获取原生 GSO 收益。

候选将内核发送交给锁定的 quinn-udp 0.5.15，通过 Tokio `try_io(WRITABLE)` 保持可写等待语义，
使用 `try_send` 保留硬错误。库中的 `send` 会把某些错误转为成功，本候选不采用该入口。
Linux 实现以一次 sendmsg 提交整个 transmit，没有新增逐段发送循环。

另一个约束是 `UdpSocketState::new` 会尝试开启 GRO，而共享接收器没有 GRO 拆分契约。
候选仅允许在 bind 前初始化，随后关闭并读回 GRO；拒绝已经绑定的 socket，避免短暂改变活跃接收路径。
组件计划验证 IPv4/IPv6 分段正文与来源、GRO 关闭、Tokio 初始注册的就绪等待与恢复和 EMSGSIZE 的错误及无投递行为。
本地仅生成锁文件和格式检查，Rust 构建与真实内核控制交给 Actions。

待完成：组件运行证据、生产接口接入、混合 UDP/QUIC 端口、XDP channel 路径及独立节点对比。
AF_XDP 不具有该内核 GSO socket 能力，后续接入必须显式保留其发送所有权和背压合同。
组件通过也不能代表节点已加速，更不能代表真实 NIC 或零复制收益。经审批的功能降级：无。

[源码与运行说明](../scripts/perf/shared-quic-tx/README.md)


初版就绪控制人为返回 WouldBlock，不符合 Tokio try_io 的前提，不能作为背压证据。
现改为在 current-thread runtime 尚未处理新 socket 的初始就绪时观察 WouldBlock，再等待真实可写事件；
每个地址族另有十秒看门狗。该控制不宣称已覆盖真实内核发送缓冲区耗尽。
