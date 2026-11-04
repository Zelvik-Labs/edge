# tokio-quiche 替换可行性核对（2026-11-02）

结论：值得建立同功能候选后端，再由性能和正确性结果决定是否替换。当前没有将 tokio-quiche 接入 Edge、编译或压测；下面是发布包源码核对，不能视为迁移已经完成或性能胜出。

## 核对版本

从 crates.io 查询并下载未撤回的 `tokio-quiche 0.20.1`，发布时间为 2026-10-13。发布包 SHA-256：`39272b0b2b311cc3e5cab36d9094bfa8e7ebc9725b4b2b4cb7a52aa57f17f075`，与注册表 checksum 一致。它依赖 quiche `0.30.0` 和 boring / boring-sys `>=4.19,<6`。正式实现应固定经验证的锁文件，并复核届时版本。

发布包的 VCS 元数据记录 commit `925b225825e2188f8e58e359114150500557bf61`。关键接口可在该提交核对：[服务端追加 HEADERS 处理](https://github.com/cloudflare/quiche/blob/925b225825e2188f8e58e359114150500557bf61/tokio-quiche/src/http3/driver/server.rs#L187)、[H3 hook 参数可见性](https://github.com/cloudflare/quiche/blob/925b225825e2188f8e58e359114150500557bf61/tokio-quiche/src/http3/driver/hooks.rs#L40)、[TLS hook](https://github.com/cloudflare/quiche/blob/925b225825e2188f8e58e359114150500557bf61/tokio-quiche/src/quic/hooks.rs#L33)、[监听器 socket 类型](https://github.com/cloudflare/quiche/blob/925b225825e2188f8e58e359114150500557bf61/tokio-quiche/src/socket/listener.rs#L52)。

同时核对了上游 master，但以下关键判断以 0.20.1 发布包为准。README 的部分示例及 feature 说明落后于实际代码：当前服务端请求通过 `ServerH3Event::Headers` 交付；默认 feature 包含 qlog-gzip / qlog-zstd；`zero-copy` 已标为兼容 feature，不能当作通用零复制开关或吞吐保证。

来源：[发布版本](https://crates.io/crates/tokio-quiche/0.20.1)、[发布包](https://static.crates.io/crates/tokio-quiche/tokio-quiche-0.20.1.crate)、[官方仓库](https://github.com/cloudflare/quiche/tree/master/tokio-quiche)。下载包、注册表响应与上游文件摘要在本工作树 `.local/h3-stage2/tokio-quiche-review/`。

## 相对当前栈的潜在价值

当前是 Quinn + Hyperium h3 + h3-quinn，再经 Edge 自有适配进入 Pingora。tokio-quiche 提供同一项目维护的 QUIC、H3 及 Tokio 驱动，可减少跨项目适配工作，并提供原生 UDP socket 能力探测、批收发和 GSO 等 I/O 集成。`IncomingH3Headers.read_fin` 也为请求开始时的正文状态提供直接输入。

这些是候选价值，不是已经测得的速度提升。它仍有每连接任务、流内有界 channel、控制/事件 channel、分配和 TLS 成本。当前 Edge 的共享分流和治理开销属于自有路径，换库后若保留相同路径，相关成本仍然存在。

若先替换下游 H3，现有 H3 回源连接池仍依赖 Quinn/h3/h3-quinn；H1/H2 的 TLS 也继续使用 rustls。因此初期会保留两套 H3 依赖和 BoringSSL / rustls 两条 TLS 路径，不能把这一步描述为已彻底精简依赖。回源迁移需要另行验证连接池、证书校验、取消与超时。

## 接口和行为差异

| 项目 | 发布包源码事实 | Edge 的接入要求 |
| --- | --- | --- |
| UDP 入口 | `socket/listener.rs` 的 `QuicListener.socket` 为具体 Tokio UdpSocket；标准 listen 返回该 socket 的连接流 | 纯 H3 专用原生 socket 可先做候选；现有混合转发及 AF_XDP 的共享 channel 不能直接替换类型。不得通过关闭这些路径伪造完整迁移 |
| 自定义 socket | `InitialQuicConnection::new` 是 pub(crate)，标准入口不能直接注入现有 SharedQuinnUdpSocket | 需要核对公开扩展面或独立应用驱动，不能假定实现一个 socket trait 就能接入整个 server listener |
| 动态证书 | `quic/hooks.rs` 的 ConnectionHook 可以创建 BoringSSL SslContextBuilder，初始化一次后复用 | 实现 SNI 回调，覆盖默认/通配符/热更/OCSP；当前 rustls CertifiedKey 不能直接传入 BoringSSL。应在配置更新时预构建 BoringSSL 证书/私钥对象并原子发布，避免每次握手导出、复制并重新解析 PEM |
| 正文与 FIN | IncomingH3Headers 提供 read_fin，InboundFrame::Body 提供 BytesMut 与 fin | 只有真实 FIN 才能证明正文为空；中途断开、reset、正文与 Content-Length 矛盾仍要保留错误，不把 channel 关闭当作干净 FIN |
| trailers | 默认 ServerHooks.handle_request 明确忽略同流后续 HEADERS（包括 trailers）；InboundFrame 没有 trailer 变体 | 当前 Edge 会将带 trailers 的请求排除共享缓存。直接使用默认驱动无法完整保留这项判断，需要补足协议事件可见性；不能将“没看到 trailers”当成“不存在 trailers” |
| H3 hook 扩展 | DriverHooks 虽公开，但 headers_received 参数 InboundHeaders 为 pub(crate)，并非可直接外部实现的稳定扩展面 | 应评估 ApplicationOverQuic 自有 H3 驱动，或等待/使用上游公开事件接口；本仓规则禁止直接修改 vendor 来绕过接口 |
| 资源边界 | H3 驱动有流内有界队列，同时控制和事件使用 unbounded channel | 应接入现有连接/请求 permit、字节账本和背压，验证慢消费者与取消；不得把上游默认值等同于 Edge 的资源保护 |
| Retry / 握手 | QuicSettings 的 disable_client_ip_validation 是监听器设置，默认要求客户端执行 stateless Retry | Edge 当前还按压力和策略控制 Retry 与握手准入；需核对公开接入点。新连接比较必须记录并统一 Retry 行为，不能把多一次往返误算成协议实现差距 |
| 请求限制 | max_requests_per_connection 是每连接请求总量设置 | 不能将它直接替代当前“并发请求数”限制；二者分别验证 |
| TLS / 构建 | 当前 H3 使用 rustls/aws-lc，候选使用 BoringSSL | 同一证书不意味着相同密码协商；固定密码组/密钥交换组后再比较握手。还要记录二进制体积、构建依赖和 TLS 双栈成本 |

## 建议实施边界与验收

先保持 Pingora 请求处理与缓存实现一致，为纯 H3 端口建立独立候选入口，复用当前证书/配置快照、请求准入和统计接口。默认栈保持可用，明确标注候选支持的入口范围；混合 UDP / QUIC 透传及 AF_XDP 完成对应适配后才能宣称全面替换。

第一道验收为严格证书校验、SNI 和证书热更、重复头、真实 FIN、延迟 FIN、正文、trailers、reset、读写超时、取消、慢读慢写和队列满。尤其先解决默认驱动丢弃 trailers 的可见性问题；若需要自有 ApplicationOverQuic，应把这部分成本计入候选结果。

随后用同一原生客户端独立交错测试 Quinn、tokio-quiche、Nginx 和 OpenResty：HIT/MISS/OFF、连接复用/多路复用/每请求新连接、1 KiB/32 KiB/128 KiB。记录完整失败、P99、回源次数、每成功响应 CPU、内存和队列峰值，既比较原生入口，也比较完整 CDN 请求路径。已有缓存或保护被绕开的结果不进入同功能排名。

至少保留 Quinn 共享 UDP、Quinn 原生 UDP、tokio-quiche 原生 UDP 三种入口对照。否则从共享 channel 切到原生 socket 的收益会与协议库收益混在一起，无法回答“差距究竟来自库还是 Edge 自有 I/O 架构”。协议功能与资源保护相同的完整路径结果，才用于是否替换的决策。

最终决定依赖这些实测。现有 H3 缓存及 NAT 建连修复仍保留；治理快照的轻量读取可供两个后端复用。接收批量化是否保留以[独立消融](performance-h3-receive-ablation-2026-11-02.md)为准，不预设它一定改善 HTTP 性能。持续 churn 的路由生命周期、H3 回源连接池及长期稳定性仍需单独验证。
