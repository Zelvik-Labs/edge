# NodeControl 对照（E-01 基线）

对照对象：

- **现有** `cloud-node-rust-v1`：`proto_defs/output_protos/`，运行时见 [grpc-api.md](grpc-api.md)。
- **规划** `NodeControl`：[Zelvik-Docs `03-api-contracts.md`](../../docs/docs/03-api-contracts.md) 与草稿 [`docs/contracts/proto/node_control.proto`](../../docs/docs/contracts/proto/node_control.proto)（WP-01，未冻结）。

本轮不实现 patch 状态机，也不改数据面。`IndexService.registerNode` 与规划 `RegisterNode` 同名异义。

来源：[`VERSION-MANIFEST.toml`](../VERSION-MANIFEST.toml)、[`Cargo.toml`](../Cargo.toml) `[package.metadata.zelvik]`、[`ORIGIN.md`](../ORIGIN.md)。上游 commit 只采用清单值 `631e5c76cc5605e748dc0230ffc019053b852f80`。本仓库无上游 git 历史，未从 `git log` 编造。

## 状态

| 状态 | 含义 |
| --- | --- |
| 已有 | 运行时调用，且能部分覆盖规划意图 |
| 近似 | 有 RPC 或 JSON，但服务、形状或生命周期不同 |
| 缺失 | 规划 RPC 或 envelope 字段在现有 proto 中不存在 |
| 冲突 | 同名/近名语义不同，或规划文档互相不一致 |

## RPC 总表

| NodeControl（`03-api-contracts.md` / 草稿 proto） | 现有 proto / 运行时 | 状态 | 说明 |
| --- | --- | --- | --- |
| `RegisterNode` | `index.IndexService.registerNode` | **冲突** | `RegisterNodeRequest`：`nodeId`、`nodeGroupId`、`nodeIP`；空响应。`index_service` 客户端已注释，数据面不调用。 |
| `RegisterNode` | `pb.NodeService.registerClusterNode` | **近似** | 请求仅 `name`；响应 `uniqueId`、`secret`、`endpoints`。集群发凭据，不是启动时能力/指纹注册。 |
| `RegisterNode` | `configs/api_node.yaml` + metadata `nodeid`/`token` | **近似** | 真实入驻是本地 `nodeId`+`secret`，无 Register RPC。规划还要软件版本、能力、地区、通信模式、证书指纹。 |
| `GetDesiredRelease` | `NodeService.findCurrentNodeConfig` | **近似** | 整包 `nodeJSON` + `version` / `nodeTaskVersion` / `compress` / `useDataMap`。无 `release_id`、签名摘要、有序 patch。 |
| `GetDesiredRelease` | `NodeTaskService.findNodeTasks` | **近似** | 任务列表，不是 resource patch 流。 |
| `ApplyRelease` | （无同名 RPC） | **缺失** | 节点内解析并热替换；无 prepare/validate/apply/ack。 |
| `ApplyRelease` | `NodeTaskService.reportNodeTaskDone` | **近似** | 仅 `isOk`/`error`，无阶段与幂等 token。 |
| `StreamEvents` | `NodeService.nodeStream` | **冲突** | 双向控制：节点 `ping`，控制面推任务。不是事件批量流。 |
| `StreamEvents` | 多个 unary 写接口 | **近似** | `CreateHTTPAccessLogs`、`CreateNodeLogs`、`CreateNodeValues` 等。无统一 sequence 与重放窗口。 |
| `ReportNodeHealth` | `NodeService.updateNodeStatus` | **近似** | `statusJSON` 含版本、CPU/内存/磁盘/load、流量、连接、缓存盘、`isActive`/`isHealthy`。无独立 listeners/XDP/task JSON。 |

## Envelope（草稿 `EnvelopeMeta`）

规划文档字段名：`schema_version`、`base_version`、`patch_seq`、`min_reader_version`。草稿 proto：`schema_version`、`base_version`、`patch_seq`、`min_reader_version`。

| 规划字段 | 现有 | 状态 |
| --- | --- | --- |
| `schema_version` | 无 | **缺失** |
| `base_version` | 各请求 `version` 游标 | **冲突**：游标 ≠ 签名基线 release |
| `patch_seq` | 无 | **缺失** |
| `min_reader_version` | 无 | **缺失** |
| 签名摘要 / 能力要求 | 无 | **缺失** |

## `RegisterNode` 字段

草稿：`node_id`、`software_version`、`capabilities_json`、`region`、`comm_mode`、`cert_fingerprint`。

| 规划 | 现有 | 状态 |
| --- | --- | --- |
| `node_id` | yaml `nodeId`；metadata `nodeid` | **已有**（非 RPC 体） |
| secret | yaml `secret`；metadata `token` | **已有**（规划请求无此字段） |
| `software_version` | `statusJSON.buildVersion` | **近似**（心跳，非注册） |
| `capabilities_json` | 无 | **缺失** |
| `region` | `FindNodeLevelInfo`；快照 `regionId` | **近似** |
| `comm_mode` | URL scheme；无 plaintext/tls/mtls 枚举 | **缺失** |
| `cert_fingerprint` | 无 | **缺失** |
| `accepted` | 无 | **缺失** |

草稿注释要求 `software_version` 为 `YYYY.GENERATION.MM.PATCH`，与 Edge 产品 SemVer `0.1.0` **冲突**。

## `GetDesiredRelease` 字段

| 规划 | 现有 | 状态 |
| --- | --- | --- |
| `release_id` | 无；`nodeJSON.version` 为整数配置版本 | **冲突** |
| `signature_digest` | 无 | **缺失** |
| `snapshot_required` | 客户端用 `version`/`isChanged`；无服务端标志 | **近似** |
| 整包快照 | `nodeJSON` | **已有**，且为常态，与「正常只传 patch」**冲突** |
| 资源 patch 列表 | 无 | **缺失** |

## `ApplyRelease` 字段

草稿 `phase`：`prepare` \| `validate` \| `apply` \| `ack`。`04-edge-integration.md` 与 Edge 控制面计划写 **switch** / **report**。

| 规划 | 现有 | 状态 |
| --- | --- | --- |
| `release_id` | 无 | **缺失** |
| `phase` | 无 | **缺失** |
| `idempotent_replay` | 无显式 token | **缺失** |
| 任务完成 | `reportNodeTaskDone` | **近似** |

## `StreamEvents` 字段

| 规划 | 现有 | 状态 |
| --- | --- | --- |
| `sequence` | 访问日志无统一 seq | **缺失** |
| 压缩批 `payload` | unary + gzip；配置 Brotli | **近似** |
| `ack_sequence` | 空 `RPCSuccess` | **缺失** |
| 与 `nodeStream` | ping/command | **冲突** |

## `ReportNodeHealth` 字段

| 规划 | `statusJSON` / 其它 | 状态 |
| --- | --- | --- |
| `ready` | `isHealthy`、`isActive` | **近似** |
| `listeners_json` | 无 | **缺失** |
| `cache_json` | `cacheTotalDiskSize`；`CreateNodeValues` cache 项 | **近似** |
| `xdp_json` | 无 | **缺失** |
| `resource_pressure_json` | CPU/内存/磁盘/load 散字段 | **近似** |
| `task_status_json` | `FindNodeTasks` | **近似** |

## 与 Docs 的冲突点

1. Apply 阶段：`03-api-contracts.md` 与草稿 proto 为 apply/ack；`04-edge-integration.md` 与 `zelvik-control-plane-plan.md` 为 switch/report。
2. RPC 命名：`03-api-contracts.md` 为 `GetDesiredRelease` / `StreamEvents` / `ReportNodeHealth`；草稿 proto 为 `GetDesiredRelease` / `StreamEvents` / `ReportNodeHealth`。
3. 组件版本：Edge SemVer `0.1.0`；Docs `11-versioning.md` 组件层 CalVer；草稿 `software_version` 注释跟 CalVer。
4. 同步形态：规划常态 patch；现网常态整包 `findCurrentNodeConfig`。
5. `registerNode`：Index vs NodeControl 同名异义。
6. 契约源：运行时绑定 `proto_defs`；NodeControl 草稿在 Docs，尚未进入 Edge 构建。

## Fixture 路径（说明，不新增密钥文件）

| 用途 | 路径 |
| --- | --- |
| 历史 `api_node.yaml` 形状 | [configuration.md](configuration.md) 占位示例；`src/api_config.rs` 内联 YAML 单测 |
| 加载候选路径 | `src/paths.rs`：`configs/api_node.yaml` 与根目录 legacy `api_node.yaml` |
| gRPC 读形状 | [grpc-api.md](grpc-api.md)；`tests/real_api_config.rs`（需显式环境变量，默认不连真实 API） |
| 日志/统计字段 | grpc-api.md 写接口节 + `proto_defs/output_protos/models/` |

后续 patch 状态机应使用脱敏目录（建议 `tests/fixtures/control-plane/`）。v1 骨架已在 `src/node_control/`（feature `node-control`）：RegisterNode / GetDesiredRelease / ApplyRelease 阶段机、有序 patch 日志、ReportNodeHealth 结构化字段、StreamEvents 的 unary 批量+序列。不把 `IndexService.registerNode` 当作别名；对外 phase 仅 `prepare|validate|apply|ack`。迁移窗口内 `findCurrentNodeConfig` 整包路径保持不变。
