# 实际 HTTP 回调 future 的 release 布局

当前节点采样中，请求过滤和回源流水线仍有较大成本。本轮直接量化六个实际 ProxyHttp 回调的堆上 future 状态大小，
作为后续调整异步结构的依据；不从功能数量或源码长度推断它们占了多少 CPU。

probe 链接实际 EdgeProxy，以解析过的本地 HTTP/1 会话与默认上下文构造各回调，读取动态 future vtable 的具体布局后销毁。
所有回调均不 poll，不执行 WAF、缓存准入、回源或日志，不对真实用户请求调用这些函数。
使用 production 相同的 mimalloc、固定 Rust 版本与 x86-64-v2 release 构建，保留实际 probe、源码和二进制哈希。

这是结构测量，不是 CPU/耗时、分配次数、分配器实际块尺寸或节点容量。小 frame 仍可能执行很多工作，
大 frame 也不能单凭尺寸解释吞吐差距。只有在看到具体布局后才选择并测试后续结构改动。

状态：待完整 CI 与实际 release probe 结果。生产回调实现保持，未移除功能或修改 vendor。
经审批的功能降级：无。
