# H3 接收消融证据

结论见[消融报告](../../performance-h3-receive-ablation-2026-11-02.md)。上一轮完整四产品数据、失败与追加复测仍在 [H3 第二轮证据](../h3-stage2-2026-11-02/README.md)。

本轮只隔离接收批量化：baseline 为原快照/单包，batched 为轻量快照/批量，candidate 为相同轻量快照/原单包。三个版本独立重启，每状态每版本三轮，每项 10 秒；轮换三方顺序，不将不同时间的 Nginx/OpenResty 结果混入这组三方排名。

comparison 含 18 个容量场景的完整索引、可移植索引、逐运行 manifest、CSV 和汇总：17 场通过，第三轮 candidate / OFF 有 12 次 curl 28 超时，失败组不能进入有效排名。runtime-quality.json 保留逐核 steal 与负载，failures.json 保留失败详情及 CPU 限流 / 内存证据。

原执行器在第 16 场停止，initial-stopped-index.json、measurements.log 和 postprocess.log 保留初始停止。continuation-metadata.json / continuation.log 记录随后仅执行剩余两场，未重跑或替换失败数据。两个版本各追加 smoke / large / fresh 共 7 项功能校验，合计 14 项；功能校验均通过。fresh 每状态 2048 次新连接并核对实际连接数与回源数。所有原生失败均保留，不因错误或高 steal 挑选样本。

metadata.json 记录冻结源码、三版本可执行文件、客户端、控制端及初始工具摘要。continuation-tools.json 记录后续执行及归档校验脚本，原工具未修改。batch-removal.patch 明确展示单包候选只恢复原接收实现，并撤回两项专属新批量行为的测试；全部旧错误/ECN/预算/NAT 回归保留。相关测试、lint 和构建日志同目录。后续重试缓冲修复不属于本组冻结二进制。

在本目录运行 `sha256sum -c artifacts.sha256` 核对归档；在仓库根目录复算：

```bash
python3 scripts/perf/summarize_architecture_candidates.py \
  --index docs/evidence/h3-recv-ablation-2026-11-02/comparison/index.portable.json \
  --output /tmp/edge-h3-receive-recomputed
```

原始逐响应 TSV 和运行日志留在忽略目录 `.local/h3-recv-ablation` 与 `.local/h3-recv-continuation`，463 项摘要见 raw-files.sha256。运行时私钥、overrides 和二进制不归档。构建缓存使用用户授权的 cloud 配置；被测 CPU、内存、业务缓存条件维持不变，编译与测量不重叠。没有部署或发布。
