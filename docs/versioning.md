# 版本号规划

`cloud-node-rust` 是上游基石，`Zelvik-Edge` 是重新命名后的独立产品。版本策略采用 **Zelvik 统一方案**（与 API / Admin / User / Agent / Portal 一致），不再使用本仓库独立 SemVer。
> 迁移说明：`0.1.0` 仅表示代码来源 provenance（cloud-node-rust 1.2.2 @ 631e5c7）；正式版本与兼容关系以 Zelvik-Docs 的全局兼容矩阵为准。

## 版本模型

| 对象 | 方案 | 例子 | 说明 |
| --- | --- | --- | --- |
| **组件（本产品）** | CalVer `YY.M.W[.P]` | `26.10.5`、同周期补丁 `26.10.5.1` | 年份、月份、月内周次 `ceil(日 / 7)`；第四段是同周期补丁号。release tag 为 `v<组件版本>`。 |
| **协议** | `api-major.api-minor` + schema | `node.v1`、`event.schema.1` | reader/writer 字段兼容，不跟随组件补丁号。 |
| **来源 provenance** | 项目 + 版本 + commit | `cloud-node-rust 1.3.1 @ b3cdc37` | 审计、迁移和回归追踪，不参与产品版本比较。 |

## 规则

- 发行日期的周期由 `license/shared/calver.ts` 计算；1–7 日为第 1 周，8–14 日为第 2 周，依此类推，29 日起为第 5 周。正式发行前按实际日期重算。
- API/Admin 等组件属于 `suite` 桶，Edge 属于 `Zelvik-Edge` 桶；各桶独立使用本周期基础版本，已占用时递增第四段补丁号。API 发布 `26.10.5` 不会占用 Edge 的 `26.10.5`。
- 日期和补丁号不表示协议兼容性；契约切换与真实迁移要求单独记录在升级说明和兼容矩阵中。
- Cargo `version` 保持三段 `YY.M.W`，与 `VERSION-MANIFEST.toml` 的 `cargo_version` 一致；可选第四段只出现在产品版本、release tag 和安装器中。
- `.github/workflows/release.yml` 由 `v*` tag 与 `workflow_dispatch` 触发，main 分支 push 不触发构建；产物命名为 `Zelvik-Edge-<target>.tar.gz`。
- 每次套件发布生成 API/Edge/Agent/Admin/User/Portal 兼容矩阵。

## 迁移

从 `cloud-node-rust 1.x` 到 `Zelvik-Edge` 是**产品迁移**，禁止安装器按普通数值版本比较处理：必须识别谱系（`cloud-node-rust-v1`）、备份、记录来源、验证配置/数据并保留回滚路径。历史 `v0.1.x`/`v1.x` 标签与发布说明已归档至 `release-notes/archive/`，不再作为版本比较依据。

完整来源和协议字段见 [VERSION-MANIFEST.toml](../VERSION-MANIFEST.toml)。

上游一次性导入记录见 [ORIGIN.md](../ORIGIN.md)（commit 以 VERSION-MANIFEST 清单为准而非本仓 git log）。与规划 NodeControl 的 RPC/字段对照见 [node-control-mapping.md](node-control-mapping.md)。
