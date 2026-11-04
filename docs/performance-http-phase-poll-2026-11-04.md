# 实际请求阶段的 poll 诊断

此前构造探针只证明 future 的构造/销毁和帧尺寸，不能说明业务执行成本。本轮使用实际 EdgeProxy、ConfigStore、负载均衡配置和 TCP Session，执行已解析 H1 GET 的真实 request_filter 与十个私有阶段。默认构建不启用诊断代码。

四种场景为 new_ctx/销毁、完整 request_filter、逐阶段 poll 和空 ready future 的时钟校准；八轮轮换顺序、每场景每轮二十块、每块两万次。检查正常路由到 Proxy 且存在 LB；逐阶段必须全部执行、各一次 poll 且没有 Pending。测量部分输出在计时后保存，失败保留已完成块。

使用生产同款 mimalloc、release fat LTO/单 codegen unit、x86-64-v2、当前线程 Tokio，绑定一个 CPU，编译完成后测量并记录 steal。保存实际 ELF、源码、构建日志、原始块与各轮中位数。

状态：待 GitHub Actions 构建和验证，尚无数值结论。

限制：同一已解析请求复用、最小无缓存配置、没有源站 IO/响应/完整 logging 或 early_request_filter admission。每次 Instant 调用、代码布局和插桩会扰动耗时，active poll 是墙钟时间，包含抢占。时钟校准单独报告，不直接相减；阶段百分比不能相加，更不能外推节点容量或完整 WAF 成本。

仍未解决：Edge 与 nginx/OpenResty 的整体性能差距。未移除业务、未放宽校验、未部署。

经审批的功能降级：无。
