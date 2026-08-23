# CloudSidra-Edge

CloudSidra-Edge 是 **CloudSidra** 产品线里的边缘数据面。当前产品版本为 **`0.1.0`**；代码以 [`cloud-node-rust` v1.2.2](https://github.com/Zypixh/cloud-node-rust) 为基线一次性导入，上游版本只是来源信息，不是 CloudSidra 的产品版本。

后续模块（API、Admin、User、Portal、权威 DNS）都叫 **CloudSidra-***，见 [产品线](docs/series.md)。它们不会塞进本仓库。

本仓库与 GitHub 仓库名均为 **CloudSidra-Edge**（私有）。不再使用短名 `edge` 或 `cloud-node`。

## 核心特性

- 多协议边缘代理：HTTP/1.1、HTTP/2、HTTP/3、gRPC、WebSocket、TCP、TCP-TLS、UDP。
- 共享端口接入、XDP/AF_XDP 旁路、混合缓存、WAF/UAM/CC、L4 强防、内容处理、热更新、日志统计、本地监控。

## 快速开始

`configs/api_node.yaml`：

```yaml
rpc.endpoints: [ "http://127.0.0.1:8001" ]
nodeId: "your-node-id"
secret: "your-node-secret"
```

仓库为私有，请用有权限的账号下载 Release 或克隆后编译：

```bash
cargo build --release
sudo ./target/release/CloudSidra-Edge install
```

CLI 为 `CloudSidra-Edge`（start / stop / status / upgrade / xdp / ntp）。

## 文档

- [产品线](docs/series.md)
- [架构](docs/architecture.md)
- [运行时](docs/runtime.md)
- [功能](docs/features.md)
- [配置](docs/configuration.md)
- [运维](docs/operations.md)
- [gRPC](docs/grpc-api.md)
- [版本号](docs/versioning.md)
- [版本来源清单](VERSION-MANIFEST.toml)
- [基线说明](ORIGIN.md)
- [CloudSidra 控制面集成计划](docs/cloudsidra-control-plane-plan.md)
- [CloudSidra 控制面 Todo](tasks/cloudsidra-todo.md)
- [Cursor 规则](CURSOR.md)
- [0.1.0](release-notes/v0.1.0.md)
- [1.2.2](release-notes/v1.2.2.md)

## 开源协议

Apache License 2.0。
