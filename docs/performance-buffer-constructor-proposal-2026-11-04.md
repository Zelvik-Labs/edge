# 新建下游 TCP 流时设写缓冲的最小提案（已获授权并实现）

## 已有证据与目标

[同 ELF 48 格实验](performance-pingora-buffering-2026-11-04.md)证明：读 0/写 1460B 相对 0/0，H1/H1-TLS 四轮配对吞吐中位提高 10.78/10.60%，CPU/响应降低 8.54/8.56%。H2 收益小且包含负向轮次。当前没有真实 Edge 节点提升结论。

目标是用现有 Pingora 缓冲实现合并小写，保留 Edge 自有 accept、L4 防护、PROXY protocol、SNI passthrough、动态证书/监听、取消、内存准入和 Linux sendfile。API/配置不删功能，不提前声明收益。

## 为什么需要一处定向的仓库规则例外

根 [AGENTS.md §2](../AGENTS.md)规定：**“Never modify vendored or upstream code”**，列出 pingora-main/ 等路径；除非任务明确针对该处，否则不能修改。以下构造器位于该目录，必须取得针对它及相邻测试的明确授权，现仅将代码作为提案展示，没有应用到源码。

现有公开 ListenerConfig::l4_buffer 只能用于标准服务监听。TransportStack、Listeners::build 和 L4Stream::set_buffer 为 pub(crate)，不能在现有自有 TCP accept 后调用。为了一个缓冲参数迁移整套监听，会牵动动态 worker、退役、早期准入、ClientHello 探测和 passthrough 的原生 TCP/splice 接口。

普通 IO/BufWriter 包装会遮蔽具体 L4Stream，Pingora 的 H1 文件响应无法再识别并使用原有 sendfile。也不使用 unsafe 布局访问私有字段。构造器只接收全新 TCP 流，比公开可随时调用的 set_buffer 更窄：后者会取出旧 BufStream，错误时机可能丢弃已缓存读写。

## 具体改动范围

只在 pingora-main/pingora-core/src/protocols/l4/stream.rs 的 impl Stream 中添加构造器及相邻测试。原 From<TcpStream>、默认容量、set_buffer 私有性以及其他上游逻辑均保持原有语义：

```rust
/// Create a TCP stream with buffers set before any application IO.
/// Callers must flush buffered output before relying on its delivery.
pub fn from_tcp_with_buffer(stream: TcpStream, buffer: L4BufferSettings) -> Self {
    let mut stream = Self::from(stream);
    stream.set_buffer(buffer);
    stream
}
```

项目 src/http_proxy_manager.rs 的 stream_with_socket_digest 改为新构造器，选择 L4BufferSettings::new(0, 1460)，继续设置同一 socket digest。构造发生在 PROXY/SNI 检查之后；SNI passthrough 在此前转入现有原生 TCP relay，因此不改变 passthrough/splice。TLS 仍在同一 rustls acceptor 上握手，具体 L4 类型与 socket 身份均保留。

额外缓冲必须进入内存账本。项目 src/memory_governor.rs 的 HTTP 连接估算增加共享常量 1460B，调用处复用该常量；不凭旧 32KiB 估算有“余量”就把新分配隐去。不增加 64KiB 读缓冲，不改变 WAF/缓存/统计开关、持久化或准入错误处理。更保守的内存估算会在内存紧张时相应减少可准入连接，这是新分配的真实成本，需要在节点报告同时给出。

## 合入性能分支前的验证

1. 构造器的 0/0 与 0/1460 真实 loopback 读写、显式 flush 和 fragmented/pipelined 输入；原 From 默认行为保持，缓冲不可丢弃未读输入。
2. 保持 IO downcast 为原 L4Stream；Linux sendfile 实际发送时先刷出 HTTP 头，校验头/正文顺序、正文哈希及部分发送/慢读者边界。禁止退回复制路径来制造成功。
3. 实际 Edge H1/H1-TLS/H2 的小包、流式 flush、100-continue、升级、取消和慢客户端；已有 SNI/PROXY/动态监听/证书测试、内存释放测试。
4. GitHub Actions 的项目 check/test/integration/Clippy 与 Pingora 检查；构建和运行独立，继续 x86-64-v2，所有失败保留。
5. 固定最新真实 Edge 的前后实际 ELF，协议 × HIT/MISS/OFF × 复用/新连接的平衡对照；另验 Range/sendfile。报告吞吐、CPU/响应、P99、RSS/cgroup 内存、准入影响及错误；有回退则保留并重新决定默认值。

已准备：定向代码和验证范围。尚未应用：任何上游/生产源码修改。设计限制：基础实验不能保证节点收益；极小或流式写必须依靠正确 flush 边界。经审批的功能降级：无。

用户已明确授权该定向构造器及相邻测试、项目接入和内存账本；实现与最终验证见 [接入报告](performance-downstream-buffer-2026-11-04.md)。以上提案保留原始范围与测量依据，历史“尚未应用”描述仅表示当时状态。
