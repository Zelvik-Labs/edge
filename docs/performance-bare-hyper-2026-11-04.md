# Hyper 与四产品的 HTTP 基础对照

[Actions 41811512638](https://github.com/Zelvik-Labs/edge/actions/runs/41811512638) 完成 75 格、4,182,110 次有效响应，零 HTTP 错误、零 steal，50 项 wrong-CA 控制全部正确拒绝。原始 ZIP、实际输入 ELF、每份逐请求记录与源版本固定的汇总器已通过独立重算；实际 Hyper ELF 为 e975e32ce90bf2eded929c3d8e26e9a41dbb1f93c98659cf86dee25cc48b3ab7（5,211,008 字节）。此结果是基础框架对照，**尚未将 Edge 核心换成 Hyper，也不是最新 Edge 的性能收益**。

## 测量口径

五目标各自独立启动，全新源站/代理/tmpfs，H1/H1-TLS/H2 OFF/连接复用 × 五轮 Latin 位置平衡；1KiB、32 并发、每格 5 秒。构建全部完成后计时，使用相同历史 a19f6e2 实际客户端、源站、请求与完整性控制。CPU 集合按每次 runner 的实际 affinity 分配：客户端 2、源站 1、代理在至少 5 CPU 时为 2，否则为 1，彼此不共享；本次实际为 proxy=[0]、client=[1,2]、origin=[3]，即 1/2/1 CPU，逐格一致。此前草稿固定写 1/2/1 不准确，现按实际分配记录。**禁止把本轮绝对 RPS 与另一台 runner 的缓冲实验直接比较。**

固定 Edge ba474745 和裸 Pingora 3b25c01 的实际 ELF；新 Hyper 使用 Hyper 1.11.0、hyper-util 0.1.20、Tokio 1.53.1、root-lock 同版本依赖、mimalloc/rustls AWS-LC、x86-64-v2、fat LTO/单 codegen unit。显式 Tokio worker 数等于代理 CPU 配额，上游池 256/120s idle、15s connect；只有固定源站转发与 hop-by-hop 头处理，不包含 Edge 的缓存、WAF、热更新、统计或持久化。

每格保留状态/协议/长度与前后正文 SHA、TLS、实际源站请求计数、进程 starttime/CPU、cgroup CPU；核对实际运行的 Edge/Hyper/Pingora ELF，并保存 nginx/OpenResty 实际主进程 ELF。上游复用均成立。控制器 tick 中位数为 0 只说明低于本轮进程 CPU 计数分辨率，不等于零 CPU。

## 五轮各指标中位数

CPU 为整个代理 cgroup 的微秒/有效响应；CPU 占用按各自配额归一。各列独立取中位数，不能由其中两列精确重算另一列。

| 协议 | 目标 | RPS | CPU µs/响应 | P99 ms | 客户端 CPU | 代理 CPU | 源站 CPU |
|---|---|---:|---:|---:|---:|---:|---:|
| h1 | edge | 5914.41 | 161.00 | 7.566 | 48.41% | 95.14% | 40.07% |
| h1 | hyper | 14821.75 | 60.69 | 3.106 | 57.63% | 89.82% | 84.20% |
| h1 | nginx | 15796.46 | 54.88 | 2.929 | 59.77% | 86.58% | 92.01% |
| h1 | openresty | 15833.30 | 54.48 | 2.916 | 59.57% | 86.06% | 92.28% |
| h1 | pingora | 11791.69 | 77.33 | 3.886 | 55.71% | 91.30% | 67.49% |
| h1-tls | edge | 5153.80 | 181.50 | 8.261 | 55.82% | 93.46% | 36.14% |
| h1-tls | hyper | 12828.08 | 70.02 | 3.398 | 65.49% | 89.87% | 77.04% |
| h1-tls | nginx | 12186.80 | 73.80 | 3.593 | 65.12% | 90.36% | 75.26% |
| h1-tls | openresty | 12529.48 | 71.92 | 3.511 | 65.50% | 90.09% | 77.23% |
| h1-tls | pingora | 10004.76 | 90.96 | 4.321 | 64.88% | 91.27% | 61.68% |
| h2 | edge | 5910.68 | 164.90 | 7.075 | 47.48% | 97.63% | 39.24% |
| h2 | hyper | 11282.80 | 82.01 | 3.760 | 71.33% | 92.35% | 72.48% |
| h2 | nginx | 10991.61 | 81.86 | 3.836 | 72.18% | 89.98% | 71.54% |
| h2 | openresty | 11412.42 | 78.61 | 3.740 | 72.67% | 89.72% | 74.25% |
| h2 | pingora | 9511.73 | 98.72 | 4.453 | 66.57% | 94.04% | 61.54% |

裸 Hyper 在本轮 H1-TLS 的中位 RPS 高于 nginx 约 5.26%、高于 OpenResty 约 2.38%；H1 仍低于两者，H2 介于两者之间。因此只能说明一个保留很少功能的 Rust HTTP 控制具备接近竞品的基础执行成本，不能宣称整个 Edge 已超过竞品，也不能把 Edge/控制的差额全部归为可删除业务。

裸 Hyper 相对裸 Pingora 的 CPU/响应中位数在 H1、H1-TLS、H2 均更低，但两者的协议实现、连接服务与调度组织同时不同，本轮不能把差值归因某个具体库函数。nginx/OpenResty 的 H1 源站约 92% 占用，RPS 接近源站上限；不能据此断言其代理容量已经测到顶。短窗口中的小幅领先仍需更长、不同负载与配额条件复核。

## 归档、失败与复算

源码 f9a174ce265110613471ba3b2666d738fea25c58；归档 13312594848，1,125 文件，ZIP SHA-256 `1634119aa9c3e30c6f00f63b78f9089dd35d1743656b1e14ab97e0407fcbd859`。归档保留实际新 Hyper、原裸 Pingora、竞品 ELF、锁文件、构建日志、所有成功与失败记录。复核入口为 [evidence-review/review.py](../scripts/perf/evidence-review/review.py)，冻结结果见 [performance-review](evidence/performance-review-2026-11-04/verification.json)。

首轮 [41809932540](https://github.com/Zelvik-Labs/edge/actions/runs/41809932540) 在 strict Clippy 阶段因 Rust 1.99 的 never-type 推断失败，没有生成新 Hyper ELF 或容量结果。原归档 13312420545、日志和失败状态继续保留。修正仅显式指定 Authority/OriginClient 和 accept 返回类型，没有增加警告抑制或把失败变成功。

已完成：五产品的独立基础对照与原始记录重算。仍未解决：功能等价的 Edge 核心改造、H3、整体超越与长期稳定性。设计限制：固定小正文、OFF、连接复用、云短窗口；不覆盖生产缓存/WAF/动态证书等能力。经审批的功能降级：无；未部署。
