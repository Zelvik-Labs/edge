# 第二批架构优化证据

结论见[报告](../../performance-architecture-stage2-2026-11-02.md)。本归档记录功能通过、局部收益和性能回退；不表示生产容量领先或长期稳定性验收通过。

## 数据范围

| 目录 | 实际对照场景数 | 用途 |
| --- | ---: | --- |
| h1 | 36 | 1 KiB、H1 reuse、三轮、四产品版本 |
| tls-h2 | 36 | TLS H1 / H2 reuse、H2 multiplex，单轮 |
| fresh | 36 | H1 / TLS H1 / H2 fresh，默认算法，单轮 |
| cold-long | 12 | H1 冷缓存 32768 唯一键，三轮 |
| 32k | 12 | H1 32 KiB，单轮 |
| regression-recheck | 32 | 两个 Edge 版本的八类回退相关场景，追加两轮 |
| uniform-tls | 24 | TLS H1 / H2 fresh，客户端统一协议、套件和组，单轮 |
| 合计 | 188 | 188 通过功能契约，其中候选 55 个，原生客户端失败 0 |

`regression-combined` 的 68 行是原始 Edge 场景与追加复测的派生视图，不是另外 68 次实验。保留未重测的原始组，报告仅对各有三轮的组计算复测中位数。

`functional-128k` 两个场景、`functional-h3` 一个场景、`profiles-before/after` 六次用户态采样和 `tls-negotiation-*` 三次诊断均不计入容量场景数。H3 只验证 OFF，缓存 BYPASS 与 fresh 问题未修复。所有场景状态、源码/二进制一致性和样本数见 `measurement-validation.json`。

## 版本、条件与限制

- Edge 两代版本字符串均为 26.11.1，以 `metadata.json` 的 SHA-256 区分；173 项冻结源码摘要、七文件本轮差异、补丁和编译/测试日志均保留。上一批证据未改写。
- Nginx 1.28.0，运行 OpenSSL 3.0.17；OpenResty 1.27.1.2，OpenSSL 3.4.1。镜像摘要和完整构建参数在各 manifest 中。
- 原生客户端和 curl 均为 libcurl 8.14.1 / OpenSSL 3.5.7。默认 Edge 使用 X25519MLKEM768，而竞品使用 X25519；统一组通过客户端配置固定 TLS 1.3、TLS_AES_256_GCM_SHA384、X25519，RSA 2048 位证书与 RSASSA-PSS 签名。`uniform-tls/negotiation/` 保存逐场景前后握手探针。默认诊断中的 readiness 探针是明文 HTTP，没有 TLS 协商行，不用来推断算法。
- 每次只测一个产品，每个场景重启目标及 tmpfs。目标 2 CPU，父级总配额 4 CPU。高 steal、短批次、客户端负载和吞吐波动标记不剔除。相同资源配置不等于相同硬件可用时间。
- Edge 基础统计和内置保护保留，竞品没有对应业务组件；控制端计入 Edge cgroup CPU。`fixture-warnings.json` 是一个指定日志的观察，不是全场景累计统计；控制端缺少部分生产后台 RPC，有出站 TLS/Geo 更新告警。未关闭验证来掩盖错误。
- profile 的原始 manifest `images.edge` 仍指常规运行时镜像；采样驱动实际替换为 `args.profiler_image`。`runtime-image-observation.json` 补充事后观察到的本地 profiler 镜像 ID，明确不是测量时采集的镜像摘要。
- profile 是用户态包含栈采样，各组可重叠；不是绝对 CPU/响应，也不包含内核和控制端。容量计算使用整个目标 cgroup 的 CPU，不扣除空闲参考。`cpu-breakdown.json` 保存每个容量场景的用户态/内核态/总 CPU 每成功响应。
- 没有完成 1/24/72 小时稳定性、真实磁盘、故障矩阵或 AF_XDP 硬件验收；没有部署或发布。

## 核对与复算

在本目录校验归档：

```bash
sha256sum -c artifacts.sha256
```

在仓库根目录用可移植索引复算一组，不覆盖已有证据：

```bash
python3 scripts/perf/summarize_architecture_candidates.py \
  --index docs/evidence/architecture-stage2-2026-11-02/h1/index.portable.json \
  --output /tmp/edge-stage2-h1-recomputed
```

其他组使用对应目录的 `index.portable.json`；原始 `index.json` 保留当时本地路径，搬移后使用可移植索引。manifest 内原始环境路径仅作为溯源信息。归档中的 results/CSV 是复算后的输出，包含全部失败、异常和未知值；本次没有失败的原生传输。

重跑测量使用 `scripts/perf/compare_architecture_candidates.py` 的独立运行入口，见[工具说明](../../../scripts/perf/README.md)。`drivers/` 保存本轮调度脚本和只用于测试客户端的 OpenSSL 配置；重跑需要匹配的二进制、镜像、有效测试证书和新的输出目录，历史绝对路径不能直接当作可用资源。源码与工具哈希在 metadata 中，不凭二进制版本字符串判断同一构建。

原生逐响应 TSV、完整诊断日志及符号栈保留在工作树的 `.local/architecture-stage2/`，2128 项摘要在 `raw-files.sha256` 中；从仓库根目录运行 `sha256sum -c docs/evidence/architecture-stage2-2026-11-02/raw-files.sha256` 可核对当前工作区原始文件。归档不包含私钥、控制端凭据配置或带栈内存的 `perf.data`；搬移本目录仍能复算汇总指标，但不能脱离原始 TSV 重算逐响应分位数。
