# 第 17 轮：Kernel 与 AF_XDP UDP 路径的同场对照

**136/136 场景、2,720 万计时 RTT 独立核验通过**。相同 Edge 切到既有 Kernel 数据面后，单流吞吐为 AF_XDP 的 **2.00～2.34 倍**，16 流提高 **18.86%～34.03%**。因此当前 AF_XDP 路径的额外成本是明显差距来源。Kernel 仍比同场 Nginx/OpenResty 低约 **5.03%～15.47%**；没有整体超过基线的结论。

被测提交 `b8c72e4a7a68c3c5b62cb77f6f40c8e30c3d0a76`，分支 `perf/udp-dataplane-20261103`，[Actions 41610335175](https://github.com/Zelvik-Labs/edge/actions/runs/41610335175) 的 Test、Check/Clippy 成功，root 单元/集成、Pingora、eBPF、23 项资源/模式/汇总 Python 检查及 5 项实际 C 负载工具检查通过。源码相对 `708785c` 没有 Rust 运行时变化；本轮增加的是观测和对照框架。没有重新运行 R16 的 32 组 native，不把其 H2/H3/非法帧计数累加成本轮验证。未请求能力 job 按条件跳过。

## 数据面和测量条件

四个对象为 Edge AF_XDP、Edge Kernel、Nginx 1.28.0、OpenResty 1.27.1.2，四轮 Williams 平衡轮换，1/2 个代理 CPU 预算各独立运行 68 场景。单流计时 80,000 次，16 流每流 20,000 次，64/1,200 字节，每流预热仍为 1,000 次；含预热共 **28,356,000** 次请求/响应。加倍样本用于改善上一轮短计时问题，不筛掉任何运行。

Linux `6.8.0-1070-azure`，release/x86-64-v2，单 XSK queue、IPv4、veth SKB/copy，所有编译结束后计时。C 客户端与外部 origin 各占独立 CPU，基线匹配 worker 数与 CPU 亲和性。完整正文、序号、长度、收发数和 origin 不同源端口数均核验，无 UDP 重试；全部配置、offload 严格读回、CPU/时窗、队列/错误和清理通过。

AF 固定 worker/bridge/eager/copied。其双向 redirect 精确为总请求两倍，完整 UDP pipeline、准入及丢弃/错误计数核验。Kernel 使用已有 `proxy-smoke --kernel`，在创建普通 socket listener 前显式关闭 runtime XDP；不是只改 YAML 上游提示，也不是前端 AF/回源 Kernel 混合路径。Kernel 的 XSK 指标不存在，不填零；报告必须证明 dataplane=kernel、UDP 443 就绪、回源确实外部。接口 JSON 在 AF 测量前必须为 SKB XDP，其他对象及每次停止后必须无 XDP，遗留挂载会导致失败。

## 完整吞吐和尾延迟

RPS 为四轮绝对值中位数；变化为同轮 Kernel/AF−1 四个比例的中位数，不强求等于两个绝对中位数之比。正 RPS、负 P99 为改善。

| CPU | 流 | 字节 | AF RPS | Kernel RPS | Kernel/AF 配对 RPS | 配对 P99 | Nginx RPS | OpenResty RPS |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 64 | 7133 | 14307 | +101.61% | -44.57% | 16305 | 16157 |
| 1 | 1 | 1200 | 7073 | 14090 | +99.60% | -43.79% | 16142 | 16143 |
| 1 | 16 | 64 | 47088 | 56069 | +18.86% | -13.49% | 60724 | 60035 |
| 1 | 16 | 1200 | 44823 | 54249 | +21.10% | -14.55% | 59121 | 58988 |
| 2 | 1 | 64 | 6185 | 14191 | +129.49% | -47.11% | 16237 | 16204 |
| 2 | 1 | 1200 | 6041 | 14137 | +133.93% | -48.70% | 16082 | 16006 |
| 2 | 16 | 64 | 59581 | 75802 | +27.49% | +3.89% | 89595 | 89286 |
| 2 | 16 | 1200 | 54472 | 72932 | +34.03% | -1.08% | 81183 | 86607 |

单轮 Kernel/AF RPS 变化 **+18.45%～+135.99%**，P99 **−49.73%～+4.78%**，全部保留。2 CPU/16 流/64 字节的 P99 配对中位数回退 **+3.89%**，不能把吞吐改善说成所有尾延迟也改善。四轮不是置信区间。

计时范围 **2.417～13.329 秒**，没有短于 0.5 秒样本。快照窗口开销最大 **0.225%**，观察 CPU steal 全为 0。31/32 个 Nginx/OpenResty 16 流样本客户端 CPU ≥90%；4 个 direct 16 流校准的客户端及 origin 也达到 90%。这些场景全部保留，表中基线不是最大容量。不同 CI 宿主的绝对 RPS 差异很大，不能把与 R16 的绝对差异解释为本轮优化收益。

## 近似进程 CPU 与实际线程

代理进程树 CPU ticks 除以计时 RTT 得到下表。窗口略宽于客户端计时，包含后台工作，不覆盖记到其他任务的全部内核网络成本；这是近似代理 CPU，不是函数级采样或整机 CPU。RSS 和这些 CPU 数均按 metadata 的实际单位重算。

| CPU | 流 | 字节 | AF μs/RTT | Kernel μs/RTT | Nginx μs/RTT | OpenResty μs/RTT | Kernel 相对 Nginx RPS | Kernel 相对 OpenResty RPS |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 64 | 117.38 | 43.50 | 32.12 | 32.31 | -12.10% | -11.45% |
| 1 | 1 | 1200 | 118.12 | 44.19 | 32.44 | 32.50 | -12.59% | -12.56% |
| 1 | 16 | 64 | 21.16 | 17.83 | 16.42 | 16.58 | -7.77% | -5.03% |
| 1 | 16 | 1200 | 22.22 | 18.42 | 16.91 | 16.94 | -8.48% | -8.37% |
| 2 | 1 | 64 | 180.44 | 43.62 | 32.38 | 32.12 | -12.72% | -12.62% |
| 2 | 1 | 1200 | 182.88 | 43.81 | 32.44 | 32.56 | -12.33% | -11.61% |
| 2 | 16 | 64 | 26.23 | 25.45 | 20.17 | 20.08 | -14.66% | -15.29% |
| 2 | 16 | 1200 | 28.31 | 26.61 | 20.70 | 20.91 | -10.44% | -15.47% |

AF 单流使用 2 个代理 CPU 时，进程 CPU 成本从约 117～118 μs 增至 180～183 μs，吞吐反而下降。Kernel 单流约 44 μs，Nginx/OpenResty 约 32 μs。仅调整正文复制或 idle yielding 没有消除的差距，在这次整条数据面切换中明显缩小。

原生 `/proc/PID/task/TID/stat` 分开记录 user/system ticks、线程 starttime、名称及每次读取时窗；没有 ptrace。按两个 payload、四轮的已匹配线程分组，以下每项为独立中位数，不强求其和等于进程表的中位数：

| AF CPU/流数 | 队列 `afxdp-eup-node-` user/system μs/RTT | 唤醒 `xsk-wake-0` user/system μs/RTT | `tokio-rt-worker` 合计 user/system μs/RTT |
| --- | ---: | ---: | ---: |
| 1 / 1 | 32.00 / 43.88 | 5.38 / 21.56 | 7.38 / 7.00 |
| 2 / 1 | 51.69 / 57.94 | 5.94 / 22.00 | 14.94 / 28.25 |
| 1 / 16 | 5.61 / 8.81 | 0.28 / 0.80 | 3.72 / 2.25 |
| 2 / 16 | 7.50 / 9.80 | 0.00 / 0.00 | 5.14 / 4.53 |

16 个 AF 单流场景有一个 `tokio-rt-worker` 在两次观测间退出，CPU 差值明确为 **null/未知**，没有代成零；对应覆盖不完整标记完整保留。上表只聚合匹配身份的线程，未知行仍在证据中，不能当作完整线程归因。2 CPU/16 流唤醒线程的零来自实际匹配计数器零增量。线程名称相同不能区分不同 Tokio runtime；system ticks 也不能直接归成某一个 syscall。

结果与当前多阶段路径一致：AF 队列 reactor、空闲唤醒 worker 与代理 runtime 之间存在额外交接，批量负载降低单 RTT 的唤醒成本，但队列处理仍占主要已匹配线程 CPU。下一步应区分队列工作、调度、批处理和内核 copy-mode 成本，再减少交接；不能把所有 write 都算 eventfd、把全部差距都归给某个函数，或直接删掉背压/错误/统计来制造收益。

## 固定内存与后续优先级

AF 进程树 RSS 中位数约 **534～548 MiB**，Kernel **526～531 MiB**。AF 的 4096×2048 帧 UMEM 是 **8 MiB**，不是 512 MiB；切到 Kernel 仍有较大 RSS，不能归咎于 UMEM。计时结束、既有 idle 观测完成后才读取 smaps，保存原文、摘要、时窗及 process starttime，逐项重算 Rss/Pss/private/shared/anonymous/swap；这些读取不会污染此前计时。

smaps 的大匿名区域不能直接标成某个分配器调用点。独立的[第 18 轮](performance-waf-state-reservation-2026-11-03.md)已经实测 WAF 七表空预分配的组件成本：代表容量约 168 万/表时增量约 448 MiB，但 System 分配器/代表容量不等于本轮完整 Node 的精确归因。多进程 RSS 求和会重复共享页；Pss 与逐进程原文一并保留，不冒充私有物理内存。

当前性能差距仍未解决。此轮没有改默认数据面，Copied/Eager 保留，也未删除功能或实施功能降级。已定位的下一优先项是 H3 回源复用的生命周期错误：正常请求后的 stream 状态不满足现有入池条件，且只保留 QUIC 句柄不足以保留 H3 sender/driver。它尚未在本轮修复，需真实 H3 源站回归测试；不是本 UDP 矩阵的收益。tokio-quiche 尚未接入，也没有换库收益结论。

## 证据与限制

[独立 UDP 核验](evidence/udp-dataplane-2026-11-03/udp-validation.json)、[全部配对/RSS/近似 CPU](evidence/udp-dataplane-2026-11-03/analysis.json)、[逐线程分组和未知项](evidence/udp-dataplane-2026-11-03/thread-cohorts.json)、[质量标记](evidence/udp-dataplane-2026-11-03/quality.json)、[二进制核验](evidence/udp-dataplane-2026-11-03/binary-validation.json)、[job/step](evidence/udp-dataplane-2026-11-03/final-jobs.json)、两预算全部 runs/summary/metadata、独立脚本及真实子进程/数据破坏控制日志都保留。先解压原始 ZIP，以解压根目录、输出 JSON 和运行提交为参数运行 `validate-udp.py`，在含该提交的仓库内执行，并保留相邻的 `validate_resources.py`。

- [网络原始 ZIP](evidence/udp-dataplane-2026-11-03/udp-raw.zip)：artifact `13220674191`，76,770,451 字节，2,429 文件/209,090,939 解压字节，SHA256 `83b7270be04fb0ed30d127ed499e5e1c4f70a0162bbe156b404e412971c4a2cc`，包含全部 RTT、终端日志与 smaps 原文。
- 被测二进制 artifact `13220854028`：26,644,275 字节，6 文件/64,546,995 解压字节，SHA256 `941a6ea10a4c77a12bd4d7952ba185e4580a5cb2daf9632fa5c8e258afa1e9ce`。四个实际可执行文件的 SHA 均与两预算 metadata 相符；本地原始归档保留，未在本工作区执行，云端副本保留至 2026-11-17。报告仓库保留其清单与验证，不重复提交二进制归档。

本轮只覆盖 IPv4、单 XSK queue、veth SKB/copy、短期稳定流；没有多队列真实 NIC、AF_XDP zero-copy、HTTP/H3 吞吐、缓存容量或长期稳定性结论。单队列也限制 AF 在 2 CPU 下的扩展性。没有主分支合并、发行/部署，RKE 继续暂停。云缓存自动 GC 关闭，本地低于 6 GiB 启动余量，没有本地 Rust 编译；仅在核验原始 ZIP/全部条目后回收冗余解压目录，原始数据和恢复命令保留。
