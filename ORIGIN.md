# 历史代码来源

Zelvik Edge 是独立产品。以下内容记录代码导入与历史兼容关系，不表示本产品是 cloud-node-rust 的替代品或重命名发行版；当前产品版本以 VERSION-MANIFEST.toml 为准。

本仓库从 `https://github.com/Zypixh/cloud-node-rust` 的 **v1.2.2** 做一次性快照，不携带该仓库的 git 历史。

- 上游标签：`v1.2.2`
- 上游提交：`631e5c76cc5605e748dc0230ffc019053b852f80`
- 上游 `main` 在导入时与该标签重合

本仓库 GitHub：`https://github.com/Zelvik-Labs/edge`。

本地继续维护的 `Documents/project/cloud-node-rust` 工作副本未被修改。

2026-09：本仓库已配置 `upstream` 远程（`Zypixh/cloud-node-rust`），并改用 merge 方式持续同步上游 `main`（见 `chore: sync upstream cloud-node-rust main` 合并提交）。产品侧已统一更名为 `zelvik-edge`，版本策略切换为 Zelvik 套件统一方案（见 `docs/versioning.md` 与 `VERSION-MANIFEST.toml`）。
