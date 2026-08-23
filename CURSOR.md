# Cursor 规则：CloudSidra-Edge

Edge 是公网数据面，修改必须优先保护现有代理、缓存、WAF、连接和资源稳定性。开始任务前先读取 docs/architecture.md、docs/grpc-api.md、docs/config-receive-audit.md、CloudSidra-Docs/docs/04-edge-integration.md 和 docs/cloudsidra-control-plane-plan.md。

硬约束：

1. 不在 Edge 实现用户、管理员、套餐、账单、供应商凭据或全局 GTM 事实源。
2. 保留现有 HTTP/1.1、HTTP/2、HTTP/3、gRPC、WebSocket、TCP、UDP、缓存、WAF、XDP/AF_XDP 和资源治理能力。
3. API 失联时使用最后成功配置继续服务；配置应用失败不得破坏旧配置。
4. 正常同步是资源级 patch；首次注册、恢复、缺口或版本不一致才请求完整快照。
5. patch 必须带基线版本、序列、digest、签名和能力要求；重复应用幂等，乱序/缺口拒绝。
6. 配额租约只执行 API 签发的本地策略；ClickHouse 延迟不能导致无界超额运行。
7. plaintext、TLS、mTLS 是管理员可配置模式；不得强制改变既有部署，迁移必须有兼容窗口。
8. 不提交真实 endpoint、secret、IP、证书或生产日志。

每次修改必须添加回归测试，运行 cargo fmt --check、cargo check --locked --all-targets、cargo test --all-targets；涉及性能时运行对应 benchmark 和压力矩阵，并报告实际结果。

