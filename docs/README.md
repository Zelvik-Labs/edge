# CloudSidra Edge 文档

文档描述 **v1.2.2 基线** 的当前行为，不保留更早版本的升级路径。README 只做入口，细节在本目录。

## 文档目录

- [架构概览](architecture.md)
- [运行时说明](runtime.md)
- [功能说明](features.md)
- [配置说明](configuration.md)
- [XDP/AF_XDP 旁路数据面](xdp-af-xdp.md)
- [RocksDB 到 Mace 元数据迁移](mace-migration.md)
- [mace-kv 0.1.1 静态审计](mace-0.1.1-audit.md)
- [接收配置审计](config-receive-audit.md)
- [部署与运维](operations.md)
- [RKE2 部署文档](rke2-deployment.md)
- [gRPC API](grpc-api.md)
- [版本号规划](versioning.md)

## 阅读顺序

1. [架构概览](architecture.md)
2. [配置说明](configuration.md)
3. [功能说明](features.md)
4. [运行时说明](runtime.md) 与 [部署与运维](operations.md)

## 维护原则

- 功能变更同步改文档。
- 命令必须对应当前 CLI。
- 不写真实密钥、内网地址或生产账号。
- 不追加已删除的历史 Release 说明。
