# 版本号规划

> 迁移说明：本页中的历史 `0.1.0` 仅表示当前代码来源 provenance。正式版本和兼容关系以 CloudSidra-Docs 的全局兼容矩阵为准；本文暂作为兼容入口。

`cloud-node-rust` 是上游基石，`CloudSidra-Edge` 是重新命名后的独立产品。仓库当前代码仍保留 **`0.1.0`** provenance；上游 `cloud-node-rust 1.2.2` 只作为来源和迁移识别信息。

## 三层版本模型

长期不要把「年份」当成这条 Rust 节点的 major。

| 对象 | 方案 | 例子 | 原因 |
| --- | --- | --- | --- |
| **CloudSidra-Edge 产品** | 独立 **SemVer `MAJOR.MINOR.PATCH`** | `0.1.0` → `0.1.1` / `0.2.0` / `1.0.0` | `0.1.x` 修复，`0.2.x` 新增能力，稳定控制面合同后 `1.0.0`。 |
| **上游 provenance** | 项目名 + 固定版本 + commit | `cloud-node-rust 1.2.2 @ 631e5c7` | 用于审计、迁移和回归，不参与 CloudSidra SemVer 比较。 |
| **CloudSidra 套件营销代次** | **CalVer `YYYY.MINOR`** | `2026.1` | 覆盖 Edge / API / Admin / User / DNS 等并列产品的兼容集合。 |
| **兼容声明** | 独立矩阵 | `CloudSidra-API 2026.1` 需要 `CloudSidra-Edge >= 0.1.0` + `cloud-node-rust-v1` | 年份无法表达「这个 RPC 还能不能用」。 |

不要用「2026 + 小版本」替代节点 SemVer：一年内会有多次不兼容的协议/行为变化，年份无法表达 breaking，最终还是要另写兼容表。CalVer 适合发版节奏和产品代次，不适合作为 crate 兼容合同。

从 `cloud-node-rust 1.2.2` 到 `CloudSidra-Edge 0.1.0` 是**产品迁移**，禁止由安装器按普通数值版本比较处理：必须识别谱系、备份、记录来源、验证配置/数据并保留回滚路径。本仓库首个 CloudSidra tag 使用 `v0.1.0`，后续 `v0.x.x`，稳定后进入 `v1.0.0`；`.github/workflows/release.yml` 继续使用 `v*` 触发。

上游 bug/security 修复且没有 CloudSidra 协议变化时递增产品 patch；新增兼容能力递增 minor；配置、gRPC、事件或运行时语义破坏性变化递增 major。完整来源和协议字段见 [VERSION-MANIFEST.toml](../VERSION-MANIFEST.toml)。上游一次性导入记录见 [ORIGIN.md](../ORIGIN.md)（项目 `cloud-node-rust`、标签 `v1.2.2`、commit `631e5c76cc5605e748dc0230ffc019053b852f80`，与 `Cargo.toml` `[package.metadata.cloudsidra]` 一致；本仓库不携带上游 git 历史，commit 以清单为准而非本仓 `git log`）。

与规划 NodeControl 的 RPC/字段对照见 [node-control-mapping.md](node-control-mapping.md)。
