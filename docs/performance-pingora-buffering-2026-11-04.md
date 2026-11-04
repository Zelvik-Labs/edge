# 隔离监听器缓冲的架构差异

源码确认两种入口有差别：标准 Pingora 的 UninitializedStream::handshake 调用 set_buffer，默认读 64KiB、写 1460B；Edge 的 process_plain_http_connection/stream_with_socket_digest 直接使用 Stream::from(TcpStream)，该构造器初始读写缓冲为 0/0。TLS 入口也使用自行包装的流。此前额外 H1 sendto 计数提供了追查方向，尚不能证明这个差别解释多少性能损失。

本轮用**同一个实际裸 Pingora ELF**切换四种设置，隔离读和写的影响：r0-w0、r64k-w0、r0-w1460、r64k-w1460。使用已有公开 ListenerConfig::l4_buffer API，未修改 Pingora/vendor 或生产入口。

H1/H1-TLS/H2 OFF/复用 × 四配置 × 四轮 Latin 位置平衡，共 48 格；相同代理/客户端/源站 CPU、TLS、mimalloc、连接池、请求与完整性控制，每格新进程/tmpfs，构建后测量。核对实际 /proc/1/exe 与启动打印的读写容量，要求正文/协议/长度和真实源站计数正确、上游连接复用，以及 32 项 wrong-CA 控制。保留全部失败并报告 CPU/有效响应、P99、资源瓶颈。

状态：待 GitHub Actions 构建与测量，未给节点收益结论。新增裸 Pingora 参数的默认值仍为标准库原有值；固定 3b25 历史 ELF 继续用于已建立的对照，不覆盖。

生产约束：直接套一个 IO 包装会影响 Pingora 对底层 Stream 的类型识别及 sendfile 路径；更改缓冲还需验证 flush、升级、流式响应和内存预算。先完成同 ELF 因子实验，不能为了合并写调用就绕过上述语义或修改上游私有接口。

设计限制：本轮是框架入口因子实验，业务功能、动态监听/证书等与 Edge 不等价。短窗口/固定小响应不代表长期稳定性，源站/客户端瓶颈仍单独报告，不能从结果推断生产功能可删除比例。未部署。经审批的功能降级：无。
