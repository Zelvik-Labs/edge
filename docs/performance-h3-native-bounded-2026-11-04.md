# 有界接收后的 H3 基线矩阵（2026-11-04，香港时间）

本轮验证 Linux 接收边界修正后的完整节点，仍使用旧 request workspace 容量记账。
243 个独立场景中，共享 Quinn **81/81** 通过，原生 Quinn **78/81**，tokio-quiche **27/81**。
后者所有 32 连接 reuse/fresh 场景均出现准入失败；两个连接的 multiplex 场景全部通过。
不能以失败格中成功的部分请求计算可比较吞吐，也不能把它当作后续容量账户修正的成绩。

源码 `ed01addd08abcb5718f767f13704b7d19692ec55`，
[工作流 41674585784](https://github.com/Zelvik-Labs/edge/actions/runs/41674585784)。
包含固定上游 FIN 修复、1500 B 完整包接收上限、GRO 关闭/GSO 保留、5 MiB/连接预算；
早于 `dabf21e` 的全局容量预留修正。工作流因真实传输错误正确返回失败。

## 结果及失败范围

| 后端 | 通过 | 错误场景 | 错误细节 |
|---|---:|---:|---|
| 共享 Quinn | 81 | 0 | 三轮、三正文、全部缓存/连接状态通过 |
| 原生 Quinn | 78 | 3 | 每场景 511/512 成功；单个 curl 56、HTTP 0、OpenSSL QUIC protocol error |
| 原生 tokio-quiche | 27 | 54 | 全部 reuse/fresh 失败，合计 42,440 次 curl 7、HTTP 0；multiplex 全部通过 |

tokio-quiche 失败范围与 32 并发需要的容量相符：双栈 backlog 为 330 MiB，额外 29 个连接
达到 475 MiB，继续申请越过约 480 MiB 的 workspace 子预算。
该轮没有首次额度快照；具体分支由后续相同边界实现的
[74f5325 三轮准入探针](performance-h3-native-admission-2026-11-04.md)直接确认。
不能仅凭本轮 curl 错误号独立证明每一次失败都由同一分支引起。

原生 Quinn 的三个错误分别为轮次 1 的 128 KiB/OFF/fresh，以及轮次 2 的
128 KiB/HIT/fresh、32 KiB/HIT/fresh。客户端连接号为 162、34、26，错误均在收到正文前发生。
它们独立保留，未归因于 tokio-quiche 的准入问题或旧 FIN 缺陷，也未改超时规避。

## 可比较的吞吐

[完整 27 格比较表](evidence/h3-native-bounded-2026-11-04/verified-native.md)
与 [243 条场景 CSV](evidence/h3-native-bounded-2026-11-04/verified-cases.csv) 保留全部结果。
下表只摘录三后端都通过的 HIT/multiplex，单元格为三轮中位数 `RPS / CPU µs/成功请求`：

| 正文 | 共享 Quinn | 原生 Quinn | 原生 tokio-quiche |
|---|---:|---:|---:|
| 1 KiB | 15,612 / 47.49 | 16,803 / 44.28 | 13,694 / 62.29 |
| 32 KiB | 4,793 / 196.40 | 5,941 / 109.73 | 5,634 / 125.75 |
| 128 KiB | 1,276 / 710.22 | 1,433 / 445.64 | 1,536 / 416.45 |

共享与原生 Quinn 的差异继续支持优化共享 UDP 路径，中大对象的每请求 CPU 更低。
tokio-quiche 在 128 KiB/multiplex 有较好结果，在小对象更贵，且 32 连接模式没有通过，
因此不具备全面替换依据。不同 runner 的绝对 RPS 不与前轮报告直接相减，
接收 GRO 的独立性能代价也不能由跨主机前后值归因。

## 条件、内存及复核

同一 feature-enabled 生产节点可执行文件切换三个后端，统一 matched 配置、地址验证 Off、
1 CPU/2 GiB、两个客户端 CPU、一个 H1 源站 CPU。三轮轮换后端顺序，每格新进程、新缓存。
H3 × OFF/MISS/HIT × reuse/fresh/multiplex × 1/32/128 KiB，完整正文/TLS/缓存/回源断言保留。
实际二进制和客户端字节已对齐所有 manifest；参数边界见[后端说明](h3-transport-backends.md)。

共重算 2,289,038 个成功测量传输，186 格通过、57 格流量错误，没有场景级超时。
126 个限量批次不足 1 秒，全部已记录 host steal 为零；部分 H3 命中窗口客户端接近饱和。
[972 个逐进程 RSS 观测](evidence/h3-native-bounded-2026-11-04/verified-process-rss.md)完整，
无缺失。1 KiB/HIT/multiplex 结束时 Edge 主进程 RSS 中位数依次为
159.84、162.23、158.15 MiB；这不是峰值、PSS、逻辑预留或优化前后差值。

原始测量 artifact `13253776235`，38,456,507 B，SHA-256
`2dbee0c50d4a63f71982883e47130694a17d09d701c12a4390da1391f9d675bd`，3065 个文件全部核验。
二进制 artifact `13253131682`，21,010,751 B，SHA-256
`8b5a347243774b87ea2c0dec2035fa1cff394d996da548177ff7d6bffba9aad4`。
实际 Edge SHA-256 为 `3c2981b02541025c5c0538b8404a6557432b18408974f788bd7b205651c60837`。
两 artifact 当前到期日为 2027-02-01 UTC；大 ZIP 保留于工作区与 Actions，
[仓库证据](evidence/h3-native-bounded-2026-11-04)包含元数据、逐文件摘要和全部重算结果。

```bash
gh api repos/Zelvik-Labs/edge/actions/artifacts/13253776235/zip > /tmp/h3-native-bounded.zip
python3 docs/evidence/h3-native-bounded-2026-11-04/verify.py --archive /tmp/h3-native-bounded.zip
```

已完成：有界接收版本的完整独立矩阵、逐传输及实际二进制核验。
仍存在：本基线的容量分区错误；原生 Quinn 的三个独立 QUIC 错误未定位。
设计限制：短时单机、不同 TLS/offload、保守固定队列预算，原生功能尚未生产对等。
经审批的功能降级：无；未排除错误场景或降低验证标准。
