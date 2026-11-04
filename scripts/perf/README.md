# 独立 HTTP 协议验证与容量测量

`protocol_matrix.py` 支持 h1（明文 HTTP/1.1）、h1-tls（HTTPS/1.1）、h2、h3。
它先验证实际响应协议、证书验证结果、HTTP 200 和完整正文，再按需调用 OHA。
协议降级、证书错误、正文错误、原生客户端错误均记录失败并返回非零；不会自动切换协议。
没有 `--oha` 时仅验证完整性，输出不能作为容量比较。HTTP/3 使用 UDP，客户端必须位于能直接到达服务 UDP 端口的本地/隔离网络中，不能经 SSH/TCP 转发压测；不依赖 RKE。

## 完整启动与缓存/连接状态对照

`protocol_state_matrix.py` 每次只运行一个产品，同时独占一个源站和 Docker 网络，退出时销毁本次容器和匿名卷。每个协议/缓存/连接/重复场景均重启目标，并重置其 tmpfs 数据目录；记录运行前后数据盘余量，避免场景间的缓存增长、防护状态和连接状态相互污染。Edge 使用生产 `Zelvik-Edge start-internal`，由 `issue34-controller` 完成认证配置下发；测试控制端与 Edge 同处代理 cgroup，额外成本保留在记录中。控制端的 `--snapshot-overrides` 只替换测试快照的顶层字段，不修改生产程序。

矩阵包括 H1、TLS H1、H2、H3；缓存规则关闭、已预热命中、唯一新键冷未命中；持久连接、每请求新建连接，另可启用 H2/H3 多路复用。固定 32 个并发请求：持久连接为 32 连接各 1 请求，多路复用为 2 连接各 16 请求。源站固定 H1 并复用上游连接。访问日志、业务 WAF 规则、自定义指标在三个测试配置中均未启用，基础统计和内置保护保留；测试不包含业务 Lua 脚本。

新连接不是 `--no-keepalive`（它只影响 TCP keepalive）。原生客户端直接设置 libcurl `FRESH_CONNECT`、`FORBID_REUSE`，并检查每一次传输的 `NUM_CONNECTS=1`。所有模式均禁用客户端 TLS session cache，新连接执行完整握手；TLS 会话恢复另测。所有模式使用同一客户端并核对每个响应的完整字节内容、实际协议和证书；失败保留在结果中。原始传输记录压缩保存，没有丢弃失败样本。

示例（先完成构建，测量不与任何编译重叠）：

```sh
# Rust 构建仍经共享 local-rust.sh；必须使用当前同一源码的生产二进制和控制端。
/workspace/docs/scripts/workspace/local-rust.sh build --locked --release \
  --bin Zelvik-Edge --example issue34-controller
python3 -I /workspace/docs/scripts/workspace/local-storage.py run -- \
  cc -O3 -Wall -Wextra -Werror benches/support/protocol_state_client.c \
  -lcurl -o .local/protocol-state-client
python3 -I scripts/perf/test_protocol_state_matrix.py
python3 -I scripts/perf/protocol_state_matrix.py --target edge \
  --output .local/state-edge --tls .local/test-tls \
  --client .local/protocol-state-client --edge-binary target/release/Zelvik-Edge \
  --controller target/release/examples/issue34-controller --multiplex
```

测试机需要 CPU 0–4、Docker 和带 H2/H3 支持的 libcurl/开发头文件。代理固定 CPU 0–1 / 2 GiB，客户端 CPU 2–3，独立源站 CPU 4 / 256 MiB。分别以 `nginx`、`openresty` 再运行，前一任务退出后才启动下一项。镜像名称/不可变 ID、编译参数、二进制 SHA、证书算法和版本差异均需记录，不能把协议不支持或失败写成 0 QPS。

正式复算入口为 `summarize_protocol_state.py --inputs <各独立 manifest> --evidence <目录> --report <文件>`；拒绝将没有逐场景隔离的探索运行混入正式结果。`compare_h2_churn.py` 是额外诊断：将 Nginx `keepalive_requests` 设为 200，检查 H2 轮换边界，结果单列，不参与主矩阵中位数。

缓存命中要求测量窗口源站请求增量为零；干净的未命中和规则关闭场景要求源站增量等于成功响应数。冷未命中采用固定请求数和唯一 URL，限制缓存写入体积，报告实际批次时长；不能冒充长时稳态容量。完整长时稳定性仍单独验收。

配置路径使用运行节点可见的绝对路径，例如：

```json
{
  "payload": "/test/payload.bin",
  "targets": {
    "edge": {
      "h1": {"url": "http://soak.test:8080/payload.bin?dyn=1", "connect_ip": "127.0.0.1"},
      "h1-tls": {"url": "https://soak.test:8443/payload.bin?dyn=1", "connect_ip": "127.0.0.1", "ca": "/test/ca.pem"},
      "h2": {"url": "https://soak.test:8443/payload.bin?dyn=1", "connect_ip": "127.0.0.1", "ca": "/test/ca.pem"},
      "h3": {"url": "https://soak.test:8443/payload.bin?dyn=1", "connect_ip": "127.0.0.1", "ca": "/test/ca.pem"}
    }
  }
}
```

上例 TLS 端点假设客户端与服务在同一 Pod；跨 Pod 时填写实际服务 IP 和匹配的证书主机名。`connect_ip` 只改变传输地址，保留 Host/SNI。生产凭据和业务 URL 不进入测试配置。
明确不支持的服务端协议写为 `{"unsupported":"附构建参数/版本证据的原因"}`，记录为 not-supported。
超时、缺少客户端能力或证书失败不能填成服务端不支持。

```sh
python3 -I scripts/perf/protocol_matrix.py --config /test/config.json \
  --output /results/edge-32k-c32 --oha /tools/oha \
  --seconds 30 --concurrency 32 --repeats 5 --client-cpu 2
python3 -I scripts/perf/test_protocol_matrix.py
```

原生测量每次只接受一个目标；Edge、Nginx、OpenResty 分别使用独立 Job、源站和网络，前一任务清理后才启动下一项。
客户端不与 Rust 编译重叠。不要把现有 `rke2-build-perf.sh` 的旧工具链、清空 target 行为用于这轮测试。
所有目标使用匹配的功能配置、相同证书算法、相同源站协议与响应体；默认/流式缓冲分别比较。

第一阶段只改变下游协议，源站固定 H1。后续上游 H1/H2、缓存命中、慢客户端、重试和配置热更新使用独立配置，不能混在一张协议排名中。
容量矩阵：1 KiB/32 KiB/1 MiB，1/32/128 并发，每组至少五次交叉重复；另测新建连接和持久连接。
在 `zelvik-dev`、指定 CI 节点、低优先级和现有配额内运行，代理采用相同 Guaranteed CPU/内存；不占生产节点，不修改生产 Deployment。

清单保留随机种子、客户端版本/SHA、驱动和配置 SHA、协商协议、正文 SHA、每次 CPU tick 和完整失败状态。
CPU/请求、P99、RSS/FD、队列丢弃、指标计数守恒、源站/客户端余量需与容器资源记录联查。
steal 或同镜像波动过大时标记为探索性，不作领先或稳定提升验收。24–72 小时稳定性与短时容量分开报告。

## 客户端与测试证书

当前环境原有 OHA 1.16.0 没有编译 HTTP/3；帮助文本列出 3 不等于二进制具备能力。需要相同版本启用 `http3` feature，并通过真实请求验明，再记录二进制 SHA。使用共享 Rust 包装器构建，不与容量测试并行。
curl 必须在 Features 中列出 HTTP2/HTTP3；不能通过 HTTP/1 回退替代。完整性客户端和原生客户端各自验证，不能只证明其中一个可用。

测试证书使用 `python3 -I scripts/memory/certs.py --out-dir <忽略目录> --host localhost --host soak.test`，生成独立 CA 和 `CA:FALSE` 服务端证书。把 CA 直接作为服务端证书会被 Rustls 拒绝，不使用 insecure 绕过。原生请求设置 `-w` 排空到期时已经发出的请求，并保留明确的请求超时和执行超时。
# 架构差距诊断（2026-11-02）

当前生产二进制的调用栈与三个产品的 syscall 对照见
[`performance-architecture-gap-2026-11-02.md`](../../docs/performance-architecture-gap-2026-11-02.md)。
复用本文矩阵准备好的 TLS、客户端和生产启动 fixture；每次只运行一个目标，禁止与编译或其他压测重叠：

```bash
python3 scripts/perf/profile_architecture.py \
  --output .local/architecture-profile-new \
  --tls .local/perf-next/tls \
  --client .local/protocol-state-matrix/protocol-state-client \
  --seconds 20 --cold-requests 32768

python3 scripts/perf/trace_architecture.py \
  --target edge --output .local/architecture-trace-edge-new \
  --tls .local/perf-next/tls \
  --client .local/protocol-state-matrix/protocol-state-client
```

随后独立运行 `--target nginx` 和 `--target openresty`，使用各自的新输出目录。
采样使用本地诊断镜像 `edge-gap-profiler:20261101`；perf 开启 PERFMON / SYS_PTRACE，
strace 通过仅共享目标 PID namespace、无网络的临时 sidecar 附着，退出后清理。
这里不修改生产配置来消除错误；协议和正文仍严格校验。

`summarize_architecture.py` 接受 `--capacity`、`--profiles`、`--traces` 和 `--output`，
检查采样有效性、生产源码与二进制摘要、syscall 总数，分开输出诊断与原正式矩阵 CPU 成本。
**诊断 manifest 内的 RPS / elapsed / CPU 数字含工具和报告生成开销，不能作容量成绩；
包含式栈分类不能相加，strace 等待时长不是 CPU 时间。**原始 perf.data 含栈内存，
保留在忽略目录且不要上传；报告只归档符号化文本与计数。

## 架构改造的旧版／新版独立交叉比较

实现与结果见 [`performance-architecture-implementation-2026-11-02.md`](../../docs/performance-architecture-implementation-2026-11-02.md)。
后续基于上一批生产构建的增量对照见 [`performance-architecture-stage2-2026-11-02.md`](../../docs/performance-architecture-stage2-2026-11-02.md)。
`compare_architecture_candidates.py` 交叉启动冻结旧版、新版、Nginx、OpenResty，第二轮相邻反转；每个场景仍由原矩阵独立重启目标和 tmpfs。
保留旧版 H2 的错误，全部运行结束并不表示全部场景通过。运行前构建并冻结两个生产 release，停止所有编译；输出目录必须是新目录：

```sh
python3 scripts/perf/compare_architecture_candidates.py \
  --baseline /test/baseline-Zelvik-Edge --candidate target/release/Zelvik-Edge \
  --tls .local/test-tls --client .local/protocol-state-client \
  --controller target/release/examples/issue34-controller \
  --output .local/architecture-h1 --repeats 3

python3 scripts/perf/summarize_architecture_candidates.py \
  --index .local/architecture-h1/index.json --output .local/architecture-h1-summary
```

默认 1 KiB、H1、32 个复用连接、8 秒窗口、MISS 4,096 唯一键、窗口前 2 秒空闲 CPU 观测；空闲成本不扣除。
另用全新输出目录逐项运行 `--protocols h1-tls h2 --multiplex --repeats 1`、
`--states miss --cold-requests 32768 --repeats 3`、
`--payload-size 32768 --cold-requests 512 --seconds 5 --repeats 1`。
单轮 TLS/H2、大响应与有界冷缓存批次分别报告，不混为长时容量。
新连接用全新目录运行 `--protocols h1 h1-tls h2 --connections fresh --fresh-requests 2048 --repeats 1`；
每个成功响应必须使用新连接，TLS session cache 关闭。`--connections reuse fresh` 可同时选择两类连接。
H3 可使用 `--protocols h3`，配合 `--multiplex` 或 `--connections fresh` 独立对照。
旧版的缓存 BYPASS 和新连接错误必须保留，不能把无缓存回源结果标为 HIT/MISS。
H3 实现、修复范围与测量见 [`performance-h3-2026-11-02.md`](../../docs/performance-h3-2026-11-02.md)。

后续分流治理与接收批量化见 [H3 第二轮报告](../../docs/performance-h3-stage2-2026-11-02.md)；Cloudflare 候选库的发布版本、接口差异及验收条件见 [tokio-quiche 接入评估](../../docs/performance-h3-tokio-quiche-evaluation-2026-11-02.md)。后者是源码评估，尚无该后端的 Edge 性能结果。

`summarize_architecture_candidates.py` 保留失败、未知值和每次传输统计，输出 cases.csv / results.json；高 steal、吞吐离散、短于 1 秒和客户端 CPU 过高另标记。
父级 CPU 配额也须核对：本次 `800000 200000` 等于 4 CPU，即使任务使用 CPU 0–4，也不能称为 5 个独占核。
共享机器的结果不用于正式领先或长期稳定性验收。

`minimal_engine_baseline.py` 加 `examples/minimal-http-proxy.rs` 只测同依赖、allocator、Tokio H1 循环的最小转发探针；它没有完整 Edge 功能。
`benches/support/request_filter_layout.py` 从生产 ELF 提取 future 的 vtable 布局和 poll 代码长度，只能用于布局诊断，不能当成分配数或吞吐收益。
