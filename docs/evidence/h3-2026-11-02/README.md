# H3 修复与性能证据

完整结论见[H3 报告](../../performance-h3-2026-11-02.md)。测量只在编译停止后运行，每个场景独立重启目标与 tmpfs。基线为上一批 `a57ba447…`，以摘要区分同版本字符串的二进制。

## 归档布局

- `metadata.json`：冻结源码、二进制与工具摘要、构建参数；源码及 Cargo 共 173 项，另有 3 项公开的 QUIC 单元测试证书夹具文件摘要。
- `compare-h3/`：H3 1 KiB、32 连接 reuse 与 2 连接 × 16 在途 multiplex，三轮旧 Edge / 新 Edge / Nginx / OpenResty。
- `compare-fresh/`：H3 每请求新建连接，512 请求单轮对照；TLS session cache 关闭。
- `compare-32k/`：H3 32 KiB reuse 单轮对照，MISS 为 512 唯一键。
- `candidate-fresh-long/`：新 Edge 每状态 2048 次新连接的较长批次。
- `candidate-h1-h2/`：H1/H2 的 HIT、MISS、OFF 回归。
- `smoke/`、`functional-128k/`：候选 H3 功能场景，单列且不计入容量场景数。
- `diagnostics/`：旧版重现与额外日志/系统调用诊断，均不进入容量排名。`fresh-demux-baseline/route-window-rejections.json` 记录导致 NAT 新连接停滞的路由窗口拒绝；服务端 UDP 计数单列。
- `measurement-validation.json`：实际场景数、状态、已知原生失败、没有原生结果的场景及功能验证统计。旧版缓存 BYPASS 和超时保留，未知值不转成零。
- `drivers/`：调度与诊断脚本。原生容量客户端未替换成开启 verbose/strace 的诊断客户端。

各比较目录含原始索引、可移植索引、逐运行 manifest、`results.json` 和 `cases.csv`。所有版本只运行一个目标进程组，目标 CPU 0–1 / 2 CPU / 2 GiB，客户端 CPU 2–3，源站 CPU 4；父级合计配额实际为 4 CPU。CPU 指整个目标 cgroup 的 µs/成功响应，空闲参考不扣除。

相同资源配置不等于独占硬件；steal、吞吐波动、客户端负载和短批次标记均保留。默认 TLS 协商未强制一致，跨产品新连接比较包含密码库和密钥交换组差异。所有客户端仍验证证书、协议、状态和完整正文；HIT 要求窗口回源数为零，干净 MISS/OFF 要求回源数匹配成功响应。

运行时镜像、版本参数、父级资源、连接数、回源数、P99、内存和文件系统变化在 manifest 中。服务端 strace 诊断替换为 `edge-gap-profiler:20261101` 并添加 ptrace 权限；该运行不计容量，其原始 manifest 的 `images.edge` 仍是常规镜像，实际替换见归档驱动。控制端并非完整生产控制面，后台 RPC/Geo/出站 TLS 告警仍是本轮局限。

## 核对与复算

在本目录验证归档摘要：

```bash
sha256sum -c artifacts.sha256
```

在仓库根目录重新计算一组，输出到新目录：

```bash
python3 scripts/perf/summarize_architecture_candidates.py \
  --index docs/evidence/h3-2026-11-02/compare-h3/index.portable.json \
  --output /tmp/edge-h3-recomputed
```

`index.portable.json` 使用归档内相对路径；原始索引和 manifest 中保留当时的环境路径作溯源，搬移后不要求这些原始路径仍存在。

完整逐响应 TSV、诊断日志和系统调用记录留在工作树忽略目录 `.local/h3-stage1/`，摘要列于 `raw-files.sha256`。从仓库根目录运行 `sha256sum -c docs/evidence/h3-2026-11-02/raw-files.sha256` 可核对本工作区原始文件。便携归档可重算汇总指标，重算逐响应分位数仍需要原始 TSV。运行时私钥和控制端凭据配置不归档；仓库中的 `tests/testdata/h3` 是公开单元测试夹具，不能用于部署。

首个测试编译错误及修正日志、存储预算中止和重试日志一并保留。仅在实际超过 12 GiB mbx 预算后，通过 mbx 的清理/GC 接口回收闲置 oha 编译目录和冷对象；两个 Edge 编译目录保留。没有提交、推送、部署或恢复 RKE。
