# RocksDB 到 Mace 元数据迁移

运行时从 RocksDB 切换到 Mace 后，节点使用新的 `metrics.mace` 目录。它不会尝试打开或修改旧的 `metrics.db`，因为两种存储格式不兼容。

## 上线步骤

1. 停止节点，确保旧 RocksDB 在迁移期间没有写入。
2. 为 `data/metrics.db` 做文件系统级备份。
3. 把数据导入同级的新目录：

```bash
cd /path/to/zelvik-edge
migration_target="$(mktemp -d)"
trap 'cargo clean --target-dir "$migration_target"' EXIT
CARGO_TARGET_DIR="$migration_target" cargo xtask migrate-metrics-to-mace \
  --source data/metrics.db \
  --destination data/metrics.mace
```

4. 检查每个 `metrics`、`unique_ip`、`cache_meta`、`firewall`、`waf_token`、`client_agent` 和 `runtime_stats` bucket 都输出 `Imported` 或 `Verified`，且没有错误。
5. 部署包含 Mace 的新二进制后启动节点。运行时会使用 `data/metrics.mace`。

## 可恢复性与回滚

导入器以只读方式打开 RocksDB，不会删除、压缩或改写源数据库。每个 Mace bucket 在完成时会保存记录数、字节数和源数据 SHA-256；重复执行相同命令会验证已完成 bucket，未完成 bucket 会从头以幂等 upsert 继续导入。

如果导入失败，先解决错误后对相同源和目标重跑命令。不要把导入目标设在 RocksDB 源目录内，也不要把已有业务数据的 Mace 目录作为导入目标。

回滚时停止新节点，保留 `metrics.mace` 供排查，并启动仍使用 `metrics.db` 的旧二进制。由于源库未修改，回滚不需要反向转换。

## Mace 0.1.x 到 0.2.0（26.10.4.9 起）

Edge `26.10.4.8` 使用 Mace 0.2.0（btree 格式 2），不能直接打开 `26.10.4.7` 及更早版本写入的 `data/metrics.mace`、`data/telemetry/core`、`data/telemetry/detail`。`26.10.4.9` 为这些旧库执行一次离线迁移：

- 迁移先把旧库复制到 `data/.mace-migration-<uuid>/source/`，再用与 `26.10.4.7` 相同的 `mace-kv 0.1.1` + `btree-store 1.0.0` 打开副本（打开时会重放已提交 WAL；与 4.7 运行时一致，崩溃留下的不完整 WAL 尾部在副本上截断）。原目录从不以写方式打开。
- 每个 bucket 以不超过 1 MiB 的事务写入 `data/mace-v2/` 下的新库；提交后重新打开新库，逐 bucket 核对记录数与 SHA-256，并通过 `check_database` 后才把整个 staging 目录 rename 发布，同时写入 `migration.json` 回执；全部成功后再写 `migration.ready` 激活标记。
- 旧目录原样保留，回滚到旧二进制时仍可照常启动。迁移需要约为旧库大小两倍加 256 MiB 的额外空间，旧库本身不会删除。
- 每次启动都会按回执核对旧库指纹；在 `migration.ready` 出现之前还会逐 bucket 核对新库摘要，激活后则只依赖 Mace 自身的打开检查，因为服务会持续写入新记录。若回滚后旧二进制又写入了数据，会先构建并验证新的 v2 树，成功后才把上一棵归档为 `data/mace-v2.stale-<uuid>` 并切换过去；重建失败时原 v2 树保持激活，把旧库全部删除即可继续用它启动。
- 确认不再回滚、需要回收空间时，请把 `data/metrics.mace` 与整个 `data/telemetry/` 一起删除；旧库整体不存在时启动会继续使用已激活的 v2 数据。只删除其中一部分会被拒绝启动（避免把已删除的 store 重建为空），恢复旧库或把剩下的旧库也删除即可。
- 备份/恢复节点数据时必须保留 `data/mace-v2/migration.json` 与 `data/mace-v2/migration.ready`；缺少激活标记时启动会把已写入的 v2 数据当作待验证结果并拒绝使用（fail-closed）。
- 失败的迁移会在 `data/.mace-migration-*` 留下当次 staging 供排查，下一次启动重试会替换它；旧库始终是恢复来源。

服务停止时手工预迁移：

```bash
sudo systemctl stop zelvik-edge.service
sudo ZELVIK_EDGE_HOME=/opt/zelvik/edge /opt/zelvik/edge/bin/zelvik-edge storage-migrate
sudo systemctl start zelvik-edge.service
```

管理端下发的原地升级由旧二进制的 supervisor 重启服务，新二进制在打开任何存储、开始监听之前完成同一迁移；数据量越大，这段启动时间越长。systemd 的 `Type=simple` 会立即报告 active，升级守护只在 15 秒内检查服务存活：可快速判定的失败（空间不足、旧库损坏）会触发自动回滚；迁移中途失败时服务退出并由 systemd 重试，旧库与旧二进制备份均保留，可人工恢复。
