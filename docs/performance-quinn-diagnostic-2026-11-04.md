# Quinn 新连接错误：协议关闭原因诊断

[GSO 实验](performance-quinn-gso-2026-11-04.md)已经稳定复现 77 次 fresh 错误，关闭 GSO 未消除。
当前开始捕获协议关闭码与触发帧；尚未据此定位根因或宣称修复。

诊断直接复用该实验的原始 Edge 可执行文件，运行时源码固定为 `82e3cf0`。
显式诊断客户端通过 OpenSSL 公共回调观察控制帧及 stream/CRYPTO 头，不更改 TLS 验证、正文校验、超时或重试。
每个活动槽有界保留最近 64 KiB，仅在传输失败时输出；关闭原因保留长度和最多 256 字节前缀。
默认压测客户端不启用该构建开关。未修改 vendored/upstream 代码。

本地 C 编译与 ASan/UBSan 控制已通过，覆盖关闭帧、截断、62-bit varint、内存边界与公开回调。
Actions 计划依次测试 native/shared/shared/native，共 65,536 次 fresh 尝试、32 KiB HIT，使用相同 matched 参数。
复核要求实际回调运行、完整逐传输计数和二进制哈希；协议错误仍令工作流失败并保留。

诊断开销会改变时序，相关吞吐/延迟不参与容量排名。没有关闭帧或不再复现都不能当成修复证据。
当前待完成：Actions 原始协议记录、关闭原因归类与针对根因的修复。
经审批的功能降级：无；本轮仅增加测试诊断，不改生产 Edge。

[运行及复核说明](../scripts/perf/quinn-diagnostic/README.md)
