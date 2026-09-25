# 版本号规划

`cloud-node-rust` 是上游基石，`CloudSidra-Edge` 是重新命名后的独立产品。版本策略采用 **CloudSidra 统一方案**（与 API / Admin / User / Agent / Portal 一致），不再使用本仓库独立 SemVer。

## 版本模型

| 对象 | 方案 | 例子 | 说明 |
| --- | --- | --- | --- |
| **组件（本产品）** | `YY.MM.PATCH`，允许偶发第四段 `YY.MM.PATCH.x` | `26.9.0`、补丁 `26.9.0.1` | 年份、发布月份、补丁号；第四段仅作为该版本的补丁版本偶发使用。release tag 为 `v<组件版本>`，如 `v26.9.0` / `v26.9.0.1`。 |
| **协议** | `api-major.api-minor` + schema | `node.v1`、`event.schema.1` | reader/writer 字段兼容，不跟随组件补丁号。 |
| **来源 provenance** | 项目 + 版本 + commit | `cloud-node-rust 1.3.1 @ b3cdc37` | 审计、迁移和回归追踪，不参与产品版本比较。 |

## 规则

- 补丁号只修复 bug、安全问题和不改变契约的行为。
- 月份变化表示同一代内的发布月份，不自动表示破坏性变化。
- Cargo `version` 与产品版本一致（`YY.MM.PATCH` 本身即为合法 SemVer）。第四段补丁号不是合法 SemVer，只出现在 release tag 与 `VERSION-MANIFEST.toml` 中，Cargo `version` 始终保持三段。
- `.github/workflows/release.yml` 由 `v*` tag 与 `workflow_dispatch` 触发，main 分支 push 不触发构建；产物命名为 `CloudSidra-Edge-<target>.tar.gz`。
- 每次套件发布生成 API/Edge/Agent/Admin/User/Portal 兼容矩阵。

## 迁移

从 `cloud-node-rust 1.x` 到 `CloudSidra-Edge` 是**产品迁移**，禁止安装器按普通数值版本比较处理：必须识别谱系（`cloud-node-rust-v1`）、备份、记录来源、验证配置/数据并保留回滚路径。历史 `v0.1.x`/`v1.x` 标签与发布说明已归档至 `release-notes/archive/`，不再作为版本比较依据。

完整来源和协议字段见 [VERSION-MANIFEST.toml](../VERSION-MANIFEST.toml)。
