# 第 8 轮：完整节点 UDP 复用对比

真实节点没有复现第 6 轮组件约 3 倍的吞吐改善：READABLE 单流比 owned worker 快约 **13%～15%**，16 流反而慢约 **3.6%**，本轮仍落后于 Nginx/OpenResty。默认 worker 不切换。结论仅属于下述 IPv4 UDP、单 XSK 队列、SKB/copy 配置。

源码 `4b29028edb47e6a038392d066eee747e2f34d3f7` 的 [Actions 41561819976](https://github.com/Zelvik-Labs/edge/actions/runs/41561819976) 两个执行 job 均成功，含 Check/Clippy、Rust 单元/集成、Pingora 与 eBPF 门禁；未选择的独立组件 job 按配置跳过。**68/68 场景成功，5,780,000 个计时 RTT 独立复核通过**。第 9 轮 `2f65ad0` 的健康队列修复不在本次测量中，不能将这里的成绩归给后续改动。

## 实现与条件

新增严格校验正文/序号的 C 客户端与回源、计时 gate、资源快照、四轮对照和 `xdp_udp_benchmark=true` 手动 CI 输入。环境为 Linux **6.8.0-1070-azure**，CPU 0/1 用于代理，CPU 2 为客户端，CPU 3 为独立回源；控制进程也消耗 CPU。所有编译结束后才开始测量。

Edge 用同一 release 二进制对照 worker、READABLE|ERROR；Nginx **1.28.0**、OpenResty **1.27.1.2** 从官方固定 SHA256 发布包构建，没有上游补丁。Rust/C 使用 x86-64-v2，C 使用 `-O3`。实际工具链、编译日志、版本与二进制摘要均保留。

两个代理 CPU 的预算相同，分工不同：Edge 一个 XSK 队列和独立会话任务，两个基线各两个 worker。Edge 到独立回源也走 AF_XDP，基线使用内核 UDP；不能将单队列结果外推为多队列上限或协议功能成本完全相同。

请求 64/1200 字节、1/16 条流，每流仅一个未完成请求。每进程预热 1000 次/流，再测 10000 次/流；收到 GO 才计时，DONE 在样本排序/输出前写出。四轮顺序为 worker/readable/OpenResty/Nginx、readable/Nginx/worker/OpenResty、Nginx/OpenResty/readable/worker、OpenResty/worker/Nginx/readable，各目标占据每个位置一次。64 个代理场景和 4 个先行直连校准全部保留。

UDP 无重试，完整回复前缀、长度、正文、序号必须匹配。基线配置 `proxy_requests 0; proxy_responses 2147483647; proxy_timeout 60s;`，INT32_MAX 哨兵保持会话；回源源端口数必须恰好为流数，验证实际复用。每个目标启动后统一关闭 veth 的 TX checksum/TSO/GSO/GRO，保留实际特性和队列信息。

## 吞吐与延迟

四次独立进程结果的中位数，单位为完整往返次数/秒。最后一列为同轮比值变化的中位数，与两个中位数直接相除略有不同。

| 流数 / 请求字节 | Edge worker | Edge READABLE | Nginx | OpenResty | READABLE 对 worker |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 / 64 | 8,738 | 10,102 | 21,854 | 21,974 | +14.81% |
| 1 / 1200 | 8,571 | 9,767 | 21,393 | 21,472 | +13.51% |
| 16 / 64 | 83,710 | 80,687 | 107,283 | 110,612 | -3.62% |
| 16 / 1200 | 75,944 | 73,142 | 99,715 | 95,029 | -3.56% |

各次运行 P99 的中位数，单位微秒：

| 流数 / 请求字节 | Edge worker | Edge READABLE | Nginx | OpenResty |
| --- | ---: | ---: | ---: | ---: |
| 1 / 64 | 135.36 | 124.21 | 61.95 | 62.56 |
| 1 / 1200 | 139.68 | 128.04 | 63.47 | 63.39 |
| 16 / 64 | 232.27 | 242.12 | 184.00 | 171.93 |
| 16 / 1200 | 253.56 | 263.52 | 194.66 | 208.35 |

16 流 READABLE 的四轮吞吐均回退：64 字节为 **-5.59%～-2.99%**，1200 字节为 **-4.37%～-3.48%**。单流 1200 字节的 P99 配对变化从 **-59.60% 到 +59.72%**，其中位数不能解释为稳定改善；全部值保留，四轮描述离散程度，不是置信区间。

单流 READABLE 比两个基线约低 54%～55%；16 流配对中位数约低 23%～28%。这是当前负载下的差距；16 流基线已受客户端限制，不能视为最大容量。

## 资源与限制

单流代理进程树 CPU 中位数约为 worker **114%**、READABLE **105%～108%**、两个基线 **51%～52%**，100% 表示一个 CPU。16 流 Edge 约 **150%～156%**，基线约 **156%～179%**。CPU 来自略宽于客户端计时的窗口，最大快照开销 **1.11%**；计数分辨率 10ms，不能解读微小差异。网络内核工作也可能计入客户端或其他上下文，代理进程树 CPU 不能单独代表全部网络 CPU 成本。

全部观测 CPU steal 为 **0%**。以下限制均标记，未删除样本：

- 17 个场景短于 0.5 秒，包括 15 个单流基线和两个直连校准。运行耗时为 0.239～2.202 秒，尚未覆盖长稳。
- 16 个 16 流基线场景的客户端 CPU 均超过 90%；两个 16 流直连的客户端和回源也超过 90%。需提高负载工具能力或增加匹配的单代理 CPU 对照再谈容量上限。
- 直连校准在客户端命名空间内访问回源，不经过代理 veth，仅为先行工具校准，不是同时测量的容量。吞吐依次约 41,801、41,453、167,972、163,378 次/秒。
- Edge 客户端 CPU 最高约 71%，没有触发上述饱和标记，不等于所有宿主干扰都已排除。

READABLE 比 worker 少一条线程和两个 FD：单流 6/7 条线程，16 流 21/22 条，FD 为 72/74。两者空闲进程树 CPU 约 **0.45%～1.36%**，本轮没有复现整核空闲自旋；空闲仅观察 2.2 秒，不能证明长期不会发生。

## 校验与证据

独立程序读取每个客户端、回源、场景记录和全部 RTT，复算吞吐、均值、P50/P99、计时边界和四轮配对变化，68 项均通过。32 个 Edge 场景的 redirect 均恰好为含预热请求数的 **2 倍**；UDP 接收、上行发送/接收、socket ingress、下行排队和 TX 成功均恰好等于请求数。会话和回源端口数为 1/16，内嵌 UDP 回源计数为 0。

所报队列丢弃、环满、无效描述符、残留 TX、故障、拥塞/AQM 丢弃、准入拒绝均为 0；UDP 错误/拒绝/溢出及 rate-limit/map-full/SNAT 分配失败也为 0。压力是**退出报告时**的 normal，不是计时阶段的压力采样。没有将无记录当作成功次数。

源码/二进制前后摘要一致，网络清理成功。产物 `13197663832` 共 **15,747,939 字节、1,057 文件**，SHA256 **`dcda0db44a43562afbac8738345a11235311b36d48d9b4f4c396c9ac1bfa45e9`** 与 GitHub 元数据一致。

- [原始产物](evidence/af-xdp-udp-reuse-2026-11-03/ci-final-raw.zip)、[元数据](evidence/af-xdp-udp-reuse-2026-11-03/ci-final-artifact.json)、[逐文件摘要](evidence/af-xdp-udp-reuse-2026-11-03/ci-final-manifest.json)。
- [完整汇总与质量标记](evidence/af-xdp-udp-reuse-2026-11-03/ci-final-summary.json)、[环境和源码摘要](evidence/af-xdp-udp-reuse-2026-11-03/ci-final-metadata.json)、[job/step](evidence/af-xdp-udp-reuse-2026-11-03/ci-final-jobs.json)。
- [独立复算](evidence/af-xdp-udp-reuse-2026-11-03/ci-final-validation.json)、[复算程序](evidence/af-xdp-udp-reuse-2026-11-03/validate-ci.py)、[双向流水线核对](evidence/af-xdp-udp-reuse-2026-11-03/ci-pipeline-validation.json)。
- [本地记录](evidence/af-xdp-udp-reuse-2026-11-03/local-validation.zip)及[清单](evidence/af-xdp-udp-reuse-2026-11-03/local-manifest.json)：C 工具 5 项、分析 3 项、Rust lib check 通过；也保留较早开发尝试，不将其算成原生测量。

归档后将[负载源码](../benches/support/udp-bench.c)原样移至 `benches/support`：SHA256 与实测提交一致，本地重编二进制逐字节相同，5 项工具测试通过。首次重编被 6GiB 剩余空间保护拒绝，随后一次准备测试因二进制未生成而失败，执行了 0 项；修正执行顺序并无损归档历史数据后重编通过。原失败与验证见[迁移验证记录](evidence/af-xdp-udp-reuse-2026-11-03/loadgen-relocation.zip)和[结论](evidence/af-xdp-udp-reuse-2026-11-03/loadgen-move-verification.json)，不将准备失败或本地重编计入 CI 性能结果。

## 决定与后续

默认继续使用 worker。单流收益不足以覆盖 16 流回退，不能由组件结果直接升级节点默认值。下一步对同样场景做独立 CPU/系统调用归因，区分 queue→session runtime→queue 的通道/调度成本、每包路由表操作、重复校验/分配和唤醒成本。

源代码已确认 `poll_raw_once` 为临时统计完整解析并分配 payload，bridge 随后再次解析/拷贝；这是候选，尚未量化吞吐贡献。优化需保留校验和、资源准入、错误状态和配置语义。基线客户端饱和、短样本也需要处理。

没有新增 HTTP 缓存、H2/H3 或 SNI 吞吐结论，没有替换 H3 库，没有证明整体超过 Nginx/OpenResty。真实 NIC 零复制、硬件 NEED_WAKEUP、更多队列及长期故障/重载压力仍待验证，RKE 暂停。

## 缓存与功能降级

云端缓存上限随容量放宽、mbx 自动 GC 关闭，保留启动 6GiB/运行 5GiB 的可用空间底线。策略专项 4 项测试通过；本地受管缓存约 15.8GiB，其中 mbx 约 14.9GiB，超过原笔记本的 mbx 12GiB 配额。物理盘约 31.45GiB，软件放宽不增加容量。为保留构建缓存，只将历史 perf 原始文件无损 gzip 归档，核对解压 SHA 并保留恢复命令。

没有申请或实施功能降级，没有发布、部署或合并 main。
