# Zelvik Edge 文档

这个目录保存 Zelvik Edge 的长期维护文档。README 只保留项目入口信息，具体功能、运行时、配置和运维细节统一放在这里。

## 文档目录

- [架构概览](architecture.md)：说明控制面、代理运行时、缓存、WAF、日志统计和四层代理之间的关系。
- [生产级边缘节点目标架构](edge-node-production-architecture.md)：面向 XDP、防护、代理、缓存和可靠性的目标合同、故障策略与验收要求；属于待实施设计。
- [生产级边缘节点开发计划](../tasks/edge-node-production-plan.md)：34 个任务包、依赖、阶段门槛、验证证据和回退方法。
- [其他模型开发交接](../tasks/edge-node-development-handoff.md)：可复制的开发与独立审阅指令，以及任务状态和证据模板。
- [运行时说明](runtime.md)：说明进程启动、任务调度、配置热更新、证书、缓存后台任务和性能监控。
- [H3 传输后端](h3-transport-backends.md)：共享/原生 Quinn 与实验性 tokio-quiche 的实现、选择和功能边界。
- [五项性能工作的交付与边界](performance-five-priorities-2026-11-04.md)：H3 回源池、WAF 内存、UDP 实验、三产品矩阵和 tokio-quiche 的实现、收益与未解决项。
- [全程序性能优化进度](performance-program-wide-2026-11-04.md)：缓存、SNI/TCP、统计、XDP 与 io_uring 的本轮交付、收益与未解决项。
- [缓存优化后的实际 CPU 复核](performance-http-cpu-after-cache-2026-11-04.md)：实际 ba474 二进制的 18 个条件、496 万次响应与 43,074 个样本。
- [用户态/内核态 CPU 差距与 H3 客户端预算](performance-http-cpu-budget-2026-11-04.md)：从 192 格原始账本复核优化方向，识别客户端容量限制。
- [实际 HTTP 回调 future 布局](performance-http-future-layout-2026-11-04.md)：release 实际回调的状态大小，结构测量不等于 CPU 归因。
- [小头表客户端 IP 查找](performance-client-ip-scan-2026-11-04.md)：75,923 组差分、全部十二种转发头及局部代价。
- [UDP 出口候选的收益与退步](performance-udp-egress-2026-11-04.md)：两种复制策略未采纳；错误载荷方案每次拒绝少两次分配，已进入性能分支。
- [实际 HTTP CPU 归因](performance-http-cpu-2026-11-04.md)：生产二进制采样、热点与可复核证据。
- [Range 缓存预检查优化与四目标对比](performance-range-preflight-2026-11-04.md)：192 个独立单元、前后 Edge/Nginx/OpenResty 吞吐、CPU 与延迟。
- [已存响应头校验优化](performance-stored-headers-2026-11-04.md)：三万多组语义差分、组件计时及 192 格真实节点前后/Nginx/OpenResty 对比。
- [AF_XDP 自动绑核修复](performance-afxdp-affinity-2026-11-04.md)：继承 CPU 集合、真实线程控制与完整节点验证。
- [Quinn GSO 隔离结果](performance-quinn-gso-2026-11-04.md)：原生新连接故障复现、GSO 开关代价与诊断边界。
- [Quinn 新连接协议诊断](performance-quinn-diagnostic-2026-11-04.md)：有界客户端帧记录、关闭码和原始可执行文件控制。
- [统计事件维度所有权](performance-metrics-move-2026-11-04.md)：192 格、1102 万次响应，未测出稳定提升，保留退步与全部原始比值。
- [每日域名统计候选](performance-daily-domain-2026-11-04.md)：复用事件域名、减少现有 key 工作，隔离验证基数、全部计数和并发 flush。
- [Quinn 收包节奏控制](performance-quinn-receive-spacing-2026-11-04.md)：固定两端实际二进制，约束待处理 datagram 的时序并保留关停剩余。
- [Quinn 客户端互操作](performance-quinn-interop-2026-11-04.md)：同版 libcurl 两种实现与同一 OpenSSL 默认/X25519 的完整错误对照。
- [共享 QUIC 维护压力](performance-quic-maintenance-2026-11-04.md)：定时清理口径修正与 32 组原生节点复核。
- [共享 QUIC 发送等待](performance-shared-quic-writable-2026-11-04.md)：队列背压、多连接独立唤醒、关闭和取消。
- [共享 QUIC 内核 TX 候选](performance-shared-quic-tx-2026-11-04.md)：GSO 原子发送、GRO 关闭、真实错误控制与未接入边界。
- [io_uring splice 架构审计](performance-io-uring-architecture-2026-11-04.md)：强制异步路径的上游源码证据和后续实现方向。
- [SNI 分片解析的分配边界与代价](performance-sni-parser-2026-11-04.md)：不完整输入零分配、完整分片扫描代价、逐输入对照和实测原始块。
- [SNI 跨记录读取与小正文栈缓冲](performance-sni-cursor-2026-11-04.md)：四版本同机对照、大分片复制消除、完整边界结果和局部取舍。
- [TCP/SNI 转发的部分写入与取消统计](performance-relay-copy-2026-11-04.md)：漏计复现、生命周期修正、计数器分配减少和短写代价。
- [最终 H3 三后端矩阵](performance-h3-native-2026-11-04.md)：共享 Quinn、原生 Quinn、tokio-quiche 的缓存/连接对比及持续连接验收。
- [最终独立 HTTP 状态对比](performance-http-state-2026-11-04.md)：Edge/Nginx/OpenResty 的协议、缓存、连接状态、进程 RSS 和 UDP 复测；[首轮结果](performance-http-state-2026-11-03.md)保留。
- [功能说明](features.md)：逐项介绍协议代理、缓存、WAF、内容处理、日志统计、证书和多级分发能力。
- [配置说明](configuration.md)：说明 `configs/api_node.yaml`、运行目录、数据文件、缓存目录和控制面配置兼容。
- [XDP/AF_XDP 旁路数据面](xdp-af-xdp.md)：说明 Linux XDP protect/proxy 配置、CLI、回退、协议接入和集成测试。
- [RocksDB 到 Mace 元数据迁移](mace-migration.md)：说明升级存储引擎时的数据导入、校验与回滚边界。
- [mace-kv 0.1.1 静态审计](mace-0.1.1-audit.md)：对照上游 issue #1 修复，说明何时可以拆除适配层串行规避。
- [接收配置审计](config-receive-audit.md)：对照全局配置、网站配置、实际应用路径和优先级，并列出当前接收缺口。
- [部署与运维](operations.md)：说明构建、安装、systemd、系统参数、监控、日志和排障建议。
- [gRPC API 使用文档](grpc-api.md)：说明节点当前调用的控制面 gRPC 接口、请求、响应和配置应用方式。
- [Agent 工程背景](agents/project-profile.md)：给 AI agent 的模块地图、依赖信号、仓库特定优先级和检索模式；入口约束见根目录 [AGENTS.md](../AGENTS.md)。
- [禁止未经审批的功能降级](agents/no-unapproved-degradation.md)：强制工程约束，定义什么算降级、审批必须绑定什么、fail-closed 的例外边界。
- [提交与 PR 规范](agents/commit-and-pr-conventions.md)：署名禁令、提交信息格式、scope 词表、本地钩子与 CI 门禁及其已知缺口。
- [警告与 lint 策略](agents/warning-and-lint-policy.md)：警告必须消除根因、禁止屏蔽手段、存量屏蔽债务清单（数量以实测为准）、审批例外与豁免范围。- [RKE2 部署文档](rke2-deployment.md)：RKE2 集群上的部署方式。
- [NodeControl 对照（E-01）](node-control-mapping.md)：规划 NodeControl 契约与现有 gcloud RPC 的逐条对照。
- [版本号规划](versioning.md)：组件/协议/来源的版本模型与兼容矩阵入口。
- [控制面集成计划](zelvik-control-plane-plan.md)：控制面接入的分阶段计划。
- 上游来源：[`ORIGIN.md`](../ORIGIN.md)、[`VERSION-MANIFEST.toml`](../VERSION-MANIFEST.toml)。

## 阅读顺序

新接触项目时建议按以下顺序阅读：

1. 先读 [架构概览](architecture.md)，理解节点整体数据流。
2. 再读 [配置说明](configuration.md)，确认节点如何连接控制面。
3. 然后读 [功能说明](features.md)，理解各类能力边界。
4. 最后读 [运行时说明](runtime.md) 和 [部署与运维](operations.md)，用于上线和排障。

## 文档维护原则

- README 保持简洁，避免堆叠所有实现细节。
- 功能变更需要同步更新对应文档。
- 文档中的命令必须能对应当前 CLI。
- 不在文档中暴露真实节点密钥、内网地址或生产账号信息。
