# Quinn 与两种 libcurl QUIC 客户端的互操作

[协议诊断](performance-quinn-diagnostic-2026-11-04.md)观察到 OpenSSL 客户端本地发出 10 次 `KEY_UPDATE_ERROR`。
本轮将同一份 C 客户端分别链接同版本 Debian libcurl 的 OpenSSL 与 GnuTLS/ngtcp2 实现，进一步约束故障范围。
尚未产生本轮网络测试结果，不能认定某一端的 bug 或已修复。

保持原始 Edge/controller 二进制、编译参数、客户端代码、正文、请求数量、QUIC 参数和缓存状态一致。
两个 libcurl 软件包的完整 Debian 版本必须相同，否则在测试前失败；实际客户端及动态库均保留哈希与文件。
OpenSSL/GnuTLS/GnuTLS/OpenSSL 顺序各独立执行 32 KiB HIT、16,384 次 fresh 尝试，共 65,536 次计划尝试。
两种客户端均不启用诊断 trace，错误继续保留并导致工作流失败。

本地已成功编译两种客户端并验证分别加载 OpenSSL 原生 QUIC、GnuTLS + ngtcp2；这只证明构建与后端选择，
不是 H3 网络验证。Actions 负责真实生产节点、协议/正文/连接计数与回源契约的复核。
不同客户端实现不做服务端吞吐排名，也不据零错误宣称长期稳定。

仍待完成：Actions 原始数据、错误归类及与 key phase/HANDSHAKE_DONE 诊断相互验证。
经审批的功能降级：无；不更改生产传输、防护或密钥更新策略。

[运行与证据说明](../scripts/perf/quinn-interop/README.md)
