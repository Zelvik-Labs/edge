# 原生 H3 传输首轮归因矩阵（2026-11-04，香港时间）

同一生产 Edge 可执行文件切换共享 Quinn、原生 Quinn、原生 tokio-quiche，已完成
243 个独立场景。**原生 Quinn 对中大正文有明显价值；这轮 tokio-quiche 没有表现出全面优势，
且全部 27 个连续新建连接场景出现握手阶段失败，因此不适合替换默认后端。**

本报告严格对应 `a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a`、
[工作流 41665227521](https://github.com/Zelvik-Labs/edge/actions/runs/41665227521)。
它已包含 quiche FIN 修复，但早于[接收上限与预算修正](performance-h3-ingress-2026-11-04.md)，
tokio-quiche 当时仍使用库的接收 GRO 和 4 MiB/连接预算假设。
不得将该结果当作后续有界接收实现的成绩。工作流因真实流量错误正确返回失败。

## 完整性及结果

| 后端 | 通过场景 | 流量错误场景 | 具体范围 |
|---|---:|---:|---|
| 共享 Quinn | 81 | 0 | 全部通过 |
| 原生 Quinn | 80 | 1 | 首轮 128 KiB/OFF/fresh 有 1 个失败传输，其余 511 个成功 |
| 原生 tokio-quiche | 54 | 27 | reuse/multiplex 全部通过；fresh 三轮、三正文、三缓存状态均失败 |

完整的 [27 格吞吐、P99、CPU 明细](evidence/h3-native-pre-ingress-2026-11-04/verified-native.md)、
[243 条场景 CSV](evidence/h3-native-pre-ingress-2026-11-04/verified-cases.csv) 和
[逐场景进程 RSS](evidence/h3-native-pre-ingress-2026-11-04/verified-process-rss.md) 均保留。
以下仅摘录 HIT/reuse，单元格为三轮中位数 `RPS / CPU µs/成功请求`：

| 正文 | 共享 Quinn | 原生 Quinn | 原生 tokio-quiche |
|---|---:|---:|---:|
| 1 KiB | 6,103 / 150.43 | 6,255 / 144.50 | 4,222 / 219.91 |
| 32 KiB | 1,365 / 696.15 | 2,081 / 328.55 | 1,941 / 401.64 |
| 128 KiB | 452 / 2074.61 | 530 / 1094.87 | 517 / 1168.79 |

共享与原生 Quinn 使用同一 QUIC/TLS 实现，差别主要在共享 UDP demux 与原生 socket 路径。
中大正文每请求 CPU 的下降支持继续优化共享收发路径；原生 socket 同时改变了 offload 能力，
不能把差额全部归给某一把锁、一次复制或某一次系统调用。

tokio-quiche 在小对象上更贵，中大对象接近原生 Quinn。128 KiB/HIT/multiplex 的中位数
为 415/454/470 RPS，但客户端与代理都可能受限，三轮短测不能建立稳定领先结论。
这也不支持“换成 Cloudflare 的库就一定更快”。

## 条件与口径

每个场景使用新代理进程与新 tmpfs，三轮轮换后端顺序，目标之间不并跑。
正文 1/32/128 KiB，H3 × OFF/MISS/HIT × fresh/reuse/multiplex，共 81 格 × 三轮。
实际 CPU 为代理 0、客户端 1/2、源站 3，代理 1 CPU、2 GiB；源站统一 H1。
所有路径运行同一 CDN/cache handler，配置为 matched、地址验证 Off，具体参数见
[架构说明](h3-transport-backends.md)。两种 TLS 提供者及内部 offload 实现仍不同。

成功格严格验证 TLS、H3、逐字节正文、缓存状态、HIT 零回源及 OFF/MISS 回源计数。
两侧各一条连接、16 个并发流用于 multiplex；fresh 明确新建连接、完成后禁止复用、禁用 TLS 会话恢复。
共重扫 836,275 个成功测量请求；失败不填零吞吐，不参与完整单元格的中位数。
全部已记录窗口的 host steal 为零，66 个限量批次不足 1 秒。
HIT/reuse 客户端核约 95%～96% 忙，未测得服务端极限容量。

采集 972 个逐进程 RSS 观测，覆盖每场景前后各两个进程。1 KiB/HIT/reuse 的 Edge 主进程
结束时 RSS 中位数为共享 Quinn 164.82 MiB、原生 Quinn 169.77 MiB、tokio-quiche 158.14 MiB。
这些不是峰值、PSS、优化前后差值或节点承诺预算，也不含控制器 RSS。
cgroup `memory.peak` 的独立字段包含文件页和 tmpfs，不能与进程 RSS 混用。

## 连续新连接失败

tokio-quiche 的失败为 `curl_7/http_0/v_0`，客户端报告 `QUIC connection has been shut down`，
多数请求在建立成功连接前就结束。它与完整正文后缺 FIN 的旧缺陷不同。
后续在当前云工作区使用该次已核验的二进制、相同客户端及 32 并发复现：
2 GiB 时 179/512 成功，日志确认 Edge 主动执行 native admission rejection；只把内存限额提高到
4 GiB 后 512/512 通过。该诊断使用两个代理核、开启局部 debug 日志，不参与以上单核吞吐排名。

当前证据支持内存准入限制，但本报告不将具体 workspace/shared-account 分支当作已经确认的事实。
已增加首个拒绝的额度快照和独立三轮探针，以区分实时并发额度与观测周期内的累计承诺。
原生 Quinn 的单个失败也保持独立记录，不合并归因。

## 可复核证据

原始测量 artifact `13250600513`，14,683,413 B，SHA-256
`82e37064cdfaba3aa2072a2a2048f844922392f1b37801b0483dfef962925f6f`；
全部 3,065 个文件已核验。独立二进制 artifact `13250420934` 也已下载并核验，
Edge 实际 SHA-256 为 `ca5b4e2dca600455b2b324a80ac5254d00c04064654b6f896afca908043b1ccd`。
两份 artifact 当前到期日均为 2027-02-01 UTC。

大 ZIP 及可执行文件保存在工作区和 Actions，仓库保留元数据、逐文件 SHA、全部重算结果：

```bash
gh api repos/Zelvik-Labs/edge/actions/artifacts/13250600513/zip > /tmp/h3-native-first.zip
python3 docs/evidence/h3-native-pre-ingress-2026-11-04/verify.py --archive /tmp/h3-native-first.zip
```

已完成：完整生产缓存路径的三后端独立比较、每请求和实际可执行文件复核。
仍存在：tokio-quiche 的新连接失败；原生 Quinn 一次未定位失败；不能宣称整体超过竞品。
设计限制：本轮早于有界接收修正、短时单机负载、不同 TLS/offload、未覆盖原生 XDP/自适应 Retry。
经审批的功能降级：无；失败场景未被降级、排除或记为成功。
