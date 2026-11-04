# 第 16 轮：UDP 正文所有权传递与 RSS

**136/136 场景、1,360 万计时 RTT** 独立核验通过。Owned 候选减少 AF 上游中转的正文复制和每流接收缓冲，但吞吐配对中位数为 **−3.12%～+2.72%**，P99 为 **−7.70%～+24.23%**，未取得稳定收益。**默认保留 Copied、Eager、worker 和 bridge 统计**。16 流配对 RSS 中位数下降约 1.06～2.03 MiB，不代表整体内存问题已解决。

运行源码 `708785cfdc711e920a82699424d1d7648bebf378`，分支 `perf/udp-owned-payload-20261103`，[Actions 41599403687](https://github.com/Zelvik-Labs/edge/actions/runs/41599403687) 的 Test 与 Check/Clippy 成功；root 单元/集成、Pingora、eBPF、32 组 native 以及两套 UDP 对照均通过。未请求的能力 job、R14/R15 独立 helper 和其他专项步骤跳过，不算本轮验证。父版本合并仅汇集前两轮报告，没有额外运行时代码变化。

## 已实现及验证

新增仅用于验证的 `--udp-payload copied|owned`。AF socket 创建时捕获模式，Owned 传递 `Bytes`，省去上游发送复制、接收 `Bytes` 到 scratch 的复制及 scratch 到下游 channel 的复制；同时不再为该 AF 会话分配 65,535 字节 scratch。两个临时正文分配随路径一起省去，这是源码性质，未直接测量分配器调用数；旧 scratch 每流分配一次。Kernel 仍保留原缓冲，已有 slice/poll QUIC API 保留复制契约。

保留 65,535 字节接收上限、空 UDP 与 EOF 的区别、ICMP/PMTU/ECN、队列满/关闭错误、CID、activity、指标和准入。六个 Rust 回归测试覆盖实际 AF socket 通道 API 的 Copied/Owned、空包至上限及截断、所有权地址、上下游错误、PMTU 拒绝、8 槽队列 Pending/唤醒/关闭，以及 Kernel loopback；最大 65,507 字节 wire 测试限定 Linux。它们使用测试通道，不声称真实 NIC 覆盖。Kernel 与 Owned 组合在验证 CLI 中拒绝。

32 组 native 为两种 payload × 两种 idle × 两种 wake × 两种 RX 统计，各两次。独立复核 **64 次 H2、128 个 H3/QUIC 探针、128 个损坏 raw 帧统计、128 次非法应用回复超时、96 次合法 wire 回复**，32 次实际 reload、队列归零、清理与摘要均通过。已有 TCP 重试保留，不证明首次全部成功。

标准 native 回源使用 loopback，设计上走 Kernel，因此模式标签不能证明 Owned 的 AF 上游路径。该路径由上述 socket 通道测试和下述外部 origin 的完整 UDP 矩阵共同覆盖，后者核对真实双向 redirect 与 sockIngress 等计数。

本地 7 项 Python 计算测试、Bash、嵌入 Python、YAML、差异及新增 Rust 格式检查通过；旧文件既有格式差异保持。云缓存预算扩至物理容量，自动 GC 关闭；空闲低于 6 GiB 启动线，未本地编译 Rust，编译及 Rust 验证在官方 runner 完成。

## 公平条件与完整结果

Linux `6.8.0-1070-azure`，release/x86-64-v2，同一 Edge 二进制比较 Copied/Owned，固定 worker/bridge/eager。1/2 个代理 CPU 分别独立运行 68 场景，四轮 Williams 轮换，1/16 稳定流，64/1,200 字节，每流预热 1,000 次；单流计时 40,000 次，16 流每流计时 10,000 次。Nginx 1.28.0、OpenResty 1.27.1.2 使用固定官方源码、匹配 worker 数和 CPU 亲和性；origin 与 C 客户端各占独立 CPU。单 XSK queue，IPv4、veth SKB/copy；所有构建结束后计时，offload 设置严格读回。

共 **13,600,000 计时 RTT、14,756,000 次含预热请求/响应**。完整正文、序号和长度由固定 C 客户端逐条校验，无重试；origin 不同源端口数等于流数。Node 双向 redirect 恰为请求数两倍，流水线、错误/丢弃、队列、亲和性、清理和跨预算源码/二进制一致性独立核验。

RPS 为四轮绝对值中位数；变化为同轮 Owned/Copied−1 的四比例中位数，不强求等于两个绝对中位数之比。RSS 为同轮 Owned−Copied；正 RPS、负 P99、负 RSS 为改善。

| 代理 CPU | 流 | 字节 | Copied RPS | Owned RPS | 配对 RPS | 配对 P99 | Nginx RPS | OpenResty RPS | 配对 RSS Δ MiB |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 64 | 20803 | 20899 | +0.16% | +3.61% | 40309 | 38279 | -1.482 |
| 1 | 1 | 1200 | 20512 | 20440 | -1.91% | +24.23% | 37551 | 37592 | -0.816 |
| 1 | 16 | 64 | 129659 | 128344 | -1.45% | +0.08% | 205739 | 200626 | -2.025 |
| 1 | 16 | 1200 | 121078 | 120958 | +0.63% | -6.16% | 192320 | 192539 | -1.064 |
| 2 | 1 | 64 | 17661 | 18020 | -0.32% | -7.70% | 38904 | 39122 | +0.086 |
| 2 | 1 | 1200 | 17433 | 17014 | -3.12% | +9.39% | 39159 | 38884 | +0.613 |
| 2 | 16 | 64 | 156250 | 159387 | +1.09% | -0.74% | 347027 | 309317 | -1.977 |
| 2 | 16 | 1200 | 143122 | 147011 | +2.72% | -1.83% | 286427 | 336020 | -1.549 |

全部轮次保留。单轮 RPS 范围 **−27.26%～+8.46%**，P99 **−70.62%～+220.38%**；四轮不是置信区间，不能挑最好一轮或删去离群值。全矩阵无报文验证或队列失败，仍不足以证明候选的尾延迟和吞吐稳定改善，因此不切换默认路径。

计时 0.292～2.490 秒，11 个小于 0.5 秒样本中 7 个为 Nginx/OpenResty、4 个为 direct；22/32 个基线 16 流样本客户端 CPU ≥90%，另 4 个 direct 客户端、3 个 direct origin 达到该标记。所有观察 CPU steal 为 0，快照边界开销最大 1.183%，没有超过 5% 的标记。质量标记和短样本全部保留，不能把表中基线 RPS 当作最大容量。不同 CI 宿主的绝对值与 R14 差异很大，不据此计算跨轮收益。

## 架构含义与仍存在的问题

16 流 RSS 有小幅一致下降；单流 RSS 的配对中位数从下降 1.48 MiB 到上升 0.61 MiB，噪声明显。Node 代理进程树 RSS 仍约 535～549 MiB。原始页数已按 metadata 的实际 4,096 字节页转换；多进程 RSS 求和会重复共享页，不是私有/物理内存。省去的缓冲容量不等于 RSS 节省，16 流也不能推断百万流容量。

近似代理 CPU/计时 RTT：单流 Copied 约 36.0～36.3 μs（1 CPU）、57.8～58.5 μs（2 CPU），Owned 约 36.1～36.5、57.5～58.9 μs；Nginx/OpenResty 约 10.1～10.8 μs。快照窗口宽于客户端计时，包含后台工作，不涵盖记到其他任务的内核网络成本；这不是函数级归因。减少复制后整体差距仍大，继续优先区分 Kernel 代理成本与 AF_XDP 队列/运行时交接成本，并调查固定内存来源。

不能把 aggregate syscall 的 write 全算作 eventfd 或日志。下一轮线程 CPU 原型仅做了合成和真实子线程自检，尚未测 Edge，不拿它归因本轮。没有 HTTP/H3 吞吐、缓存容量、真实 NIC、零复制或长期稳定性结论，没有整体超过 Nginx/OpenResty 的结论。

## 证据与设计限制

所有 [配对/RSS/近似 CPU](evidence/udp-owned-payload-2026-11-03/analysis.json)、[质量标记](evidence/udp-owned-payload-2026-11-03/quality.json)、[UDP 独立验证](evidence/udp-owned-payload-2026-11-03/udp-validation.json)、[native 验证](evidence/udp-owned-payload-2026-11-03/native-validation.json)、[job/step](evidence/udp-owned-payload-2026-11-03/final-jobs.json)、两预算 metadata/runs/summary、逐文件 SHA 和验证脚本都在证据目录。验证脚本以解压目录、输出文件、运行源码 SHA 为参数，需在包含该提交的仓库运行。

- [UDP 原始 ZIP](evidence/udp-owned-payload-2026-11-03/udp-raw.zip)：artifact `13217081401`，32,763,836 字节、2,202 文件，SHA256 `7df64f2f0a9c251288423af9b0b979dc8df7bf59508ab49e9e114dd2384e349d`。
- [Native 原始 ZIP](evidence/udp-owned-payload-2026-11-03/native-raw.zip)：artifact `13216085158`，137,255 字节、69 文件，SHA256 `91e80a4aa7690d404ff527e5054fed7ded34b234e2318eda60d07f1ce784c851`。
- [存储摘要](evidence/udp-owned-payload-2026-11-03/storage-summary.json)：28 个同 owner/mode、逐字节一致的报告 ZIP 副本以硬链接保留所有路径和内容，实际增加 96,223,232 字节空闲；本地保留可撤销清单。没有清理构建缓存。完整归档与每个解压文件 SHA 验证后，仅回收冗余解压目录，原始 ZIP 和复原命令保留。

已实现可验证的所有权候选和 RSS 字节汇总；整体性能差距及较大固定 RSS 仍存在。默认复制路径保留，未删除功能或实施功能降级；没有 H3 库切换、主分支合并、发行/部署，RKE 继续暂停。已有故障/退役 teardown 的 `send_raw_frame(...).unwrap_or(false)` 与功能 fixture 的 best-effort offload 边界未扩展；计时 offload 严格核验。
