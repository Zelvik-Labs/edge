# H3 第二轮优化证据

结论见[报告](../../performance-h3-stage2-2026-11-02.md)，tokio-quiche 源码评估见[接入说明](../../performance-h3-tokio-quiche-evaluation-2026-11-02.md)。本轮基线为上一轮 H3 修复后的 `f1a38b900…`，没有复用旧缓存失效版本作为本轮性能基线。

`metadata.json`、baseline-source.json、candidate-source.json 和 stage2.patch 记录源码、二进制、工具与构建参数。比较目录含原始索引、可移植相对路径索引、逐运行 manifest、results.json 和 cases.csv；每个场景独立重启目标与 tmpfs。

- compare-h3：1 KiB，reuse 与 multiplex，三轮，两个 Edge 版本、Nginx、OpenResty。
- compare-fresh：每请求新建连接，512 请求，单轮三状态四产品。
- compare-32k：32 KiB reuse，单轮三状态四产品。
- recheck-reuse：初始三轮复用指标回退且 steal 差异较大，追加三组旧/新 Edge 的 HIT/OFF 逐项交错复测，共 12 个独立 10 秒场景。原始场景全部保留，复测不替换原结果。
- candidate-fresh-long：每状态 2048 新连接；candidate-h1-h2：H1/H2 三状态功能及短批次回归。
- smoke、functional-128k：候选功能场景，单列且不计容量场景数。
- profiles-before / profiles-after：两个 Edge 二进制的 H3 HIT/OFF 用户态 CPU 诊断，排除容量排名；包含栈分组会重叠，不能相加为独立 CPU 占比。
- profiles-before-initial / profiles-after-initial：初次 199 Hz 记录，基线 HIT 丢失 16 个样本；保留为不用于对照的诊断，不将丢样改成零。profiles-before-buffer-failed 保留 16 MiB 缓冲区超出内核锁页额度而失败的两项重试。最终前后四组采用一致的 99 Hz / 2 MiB 缓冲区重采，被测进程仍绑定 CPU 0–1。
- tokio-quiche-review：注册表记录、发布包摘要和核对源码的摘要；不是已接入或压测的后端。

容量运行使用未改动的原生客户端，校验证书、协议、状态和完整正文；HIT 要求零回源，MISS/OFF 核对回源数量，fresh 核对每成功请求的新连接数。默认 TLS 密码协商未强制统一，新连接跨产品对比包含 TLS 实现与协商差异。共享父级为 4 CPU，分配 CPU 编号不等于独占硬件。

在本目录验证便携归档：

```bash
sha256sum -c artifacts.sha256
```

在仓库根目录复算一组：

```bash
python3 scripts/perf/summarize_architecture_candidates.py \
  --index docs/evidence/h3-stage2-2026-11-02/compare-h3/index.portable.json \
  --output /tmp/edge-h3-stage2-recomputed
```

完整逐响应 TSV、perf.data、调用栈及运行日志位于忽略目录 `.local/h3-stage2/`；raw-files.sha256 记录原始日志和逐响应数据摘要。便携归档可复算汇总，逐响应分位数重算需要本地原始 TSV。运行时私钥、控制端 overrides 和原始 perf.data 不归档。

采样时实际使用 `edge-gap-profiler:20261101`，添加 PERFMON / SYS_PTRACE，并替换目标启动镜像；该替换在归档驱动中执行，原 manifest 的 images.edge 仍保留常规镜像名称。采样运行不进入容量表。

没有提交、推送、部署或恢复 RKE。短批次不代表长期稳定性。
