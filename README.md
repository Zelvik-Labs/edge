# CloudSidra Edge

CloudSidra Edge 是全局流量系统的**边缘节点数据面**。代码以 [`cloud-node-rust` v1.2.2](https://github.com/Zypixh/cloud-node-rust) 当前主干为基线一次性同步而来，从此在本仓库独立演进。上游 `cloud-node-rust` **不会被本仓库改动**，仍可继续单独维护。

本仓库不保留 v1.2.2 之前的发布说明与升级叙事。当前能力、配置和 gRPC 契约均以 v1.2.2 为准。

它基于 [Pingora](https://github.com/cloudflare/pingora)，面向站点加速、多级分发、动态配置、边缘安全、访问日志和统计上报。

## 与 cloud-node-rust 的关系

| | 本仓库 `CloudSidra/edge` | `Zypixh/cloud-node-rust` |
| --- | --- | --- |
| 角色 | CloudSidra 数据面基线与后续开发 | 原项目，继续独立维护 |
| 基线 | v1.2.2（提交见 [ORIGIN.md](ORIGIN.md)） | 持续发版 |
| 历史 Release | 不收录 1.2.1 及更早说明 | 完整保留 |

## 核心特性

- **多协议边缘代理**：HTTP/1.1、HTTP/2、HTTP/3、gRPC、WebSocket、TCP、TCP-TLS、UDP。
- **共享端口接入**：支持共享 `443` 端口下的 TLS、HTTP/2、HTTP/3、`@sni_passthrough` 和 `@quic` 流量分流。
- **XDP/AF_XDP 旁路**：Linux 上可显式启用 XDP protect/proxy 模式，支持 HTTP、HTTPS、TCP、UDP、SNI、QUIC 和 H3 命中端口旁路，并保留 socket fallback。
- **混合缓存系统**：Memory + Disk 缓存，Mace 元数据索引，多缓存策略匹配，支持大文件和高并发切片分发。
- **站点安全能力**：WAF、UAM、CC、防盗链、访问限制、请求限制、带宽和流量限制、自定义拦截页面。
- **L4 自适应强防**：复用 `emptyConnectionFlood` 开关覆盖 HTTP/HTTPS/TCP/SNI/UDP/QUIC/H3 的慢连接、队列和 admission 攻击防御，支持集群级 IP 上报拉黑。
- **内容处理能力**：WebP 转换、HTML/CSS/JS 优化、HLS 播放列表和分片处理、自定义错误页和模板变量。
- **动态运行时**：配置热更新、动态证书、OCSP 同步、源站健康检查、多级回源和父节点压力感知。
- **日志与统计**：访问日志、节点日志、L7/L4 指标、日统计、域名统计、Top IP、缓存命中和攻击统计。
- **本地可观测性**：内置性能监控页面，展示请求、连接、缓存、系统资源和异常趋势。

## 快速开始

准备 `configs/api_node.yaml`：

```yaml
rpc.endpoints: [ "http://127.0.0.1:8001" ]
nodeId: "your-node-id"
secret: "your-node-secret"
```

安装当前仓库 Release：

```bash
curl -fsSL https://raw.githubusercontent.com/CloudSidra/edge/main/scripts/install-rust-cloud-node.sh | bash
```

全新安装示例：

```bash
curl -fsSL https://raw.githubusercontent.com/CloudSidra/edge/main/scripts/install-rust-cloud-node.sh | bash -s -- --fresh --yes --api-endpoint http://127.0.0.1:8001 --node-id your-node-id --secret your-node-secret --geoip
```

## 文档

- [架构概览](docs/architecture.md)
- [运行时说明](docs/runtime.md)
- [功能说明](docs/features.md)
- [配置说明](docs/configuration.md)
- [XDP/AF_XDP 旁路数据面](docs/xdp-af-xdp.md)
- [RocksDB 到 Mace 元数据迁移](docs/mace-migration.md)
- [部署与运维](docs/operations.md)
- [RKE2 部署文档](docs/rke2-deployment.md)
- [gRPC API](docs/grpc-api.md)
- [版本号规划](docs/versioning.md)

基线发布说明：[1.2.2](release-notes/v1.2.2.md)

## 构建产物

Release workflow 面向不同 CPU 提供 Linux 产物：`linux-x64-v2-sse4.2`、`linux-x64-v3-avx2`、`linux-x64-v4-avx512`、`linux-arm64-generic`、`linux-arm64-neoverse-n1`。

```bash
cargo clean
RUSTFLAGS="-C target-cpu=x86-64-v3 -C opt-level=3 -C lto=fat" cargo build --release
```

## 系统要求

- Linux，推荐内核 `5.x+`。
- XDP/AF_XDP 可选，需 root 或 `CAP_BPF`、`CAP_NET_ADMIN`、`CAP_NET_RAW`，默认关闭。
- 高并发需提高 `LimitNOFILE`、`somaxconn`、`tcp_max_syn_backlog`、本地端口范围和 conntrack。
- GeoIP、缓存、证书需要正确的数据目录权限。

## 开源协议

Apache License 2.0。
