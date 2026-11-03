# 第 14 轮：空闲轮次的有界协作调度

**136/136 场景、1,360 万计时 RTT** 独立复核通过。用协作预算替代 idle select 后的额外强制让出，八档吞吐配对中位数变化 **−0.65%～+2.92%**，P99 变化 **−2.78%～+2.27%**。收益小且不一致，**默认继续使用 eager**；这不是主要性能差距的解决方案。

被测源码 `8851c119d8e0f160cb10cdb347b9acb0943c3c7d`，分支 `perf/idle-yield-20261103`，[Actions 41590512929](https://github.com/Zelvik-Labs/edge/actions/runs/41590512929) 的 Test、Check/Clippy 均成功，包含单元/集成、Pingora、eBPF、16 组 native 与两套 UDP 对照。未选择的独立能力 job 按条件跳过，不计为验证。第 15 轮 H3 位图索引是另一提交，本轮不包含其代码或收益。

## 实现与调度约束

旧队列每轮结束都 `yield_now`，包括刚从 idle select 恢复的轮次。新增验证 CLI `--idle-yield eager|cooperative`，不增加生产配置字段。候选仅在 idle select 实际完成后调用 `consume_budget`；忙轮次、短定时器自旋和仍持有 readiness guard 的路径仍每轮强制让出。保留 ring/readiness、队列背压、PMTU、TX kick、故障和重载处理。

select/Notify 可以立即就绪，因此不能直接删掉协作点。测试使用同一生产 helper，在有限 ready 循环中要求其他 task 取得进展，并检查 busy 首次 poll 必须 Pending。独立 crate 的两个故障版本分别移除 idle budget、削弱 busy yield，均恰好触发指定失败，状态 101 保留；恢复原始字节后再次通过，Clippy 通过。这证明测试能识别这两类调度错误，不证明高负载网络下的严格 timer 时限。

本地 6 项报告计算检查、Python/嵌入 Python/YAML、Bash 和新增 Rust 格式通过；旧文件格式差异与父版本相同。云缓存预算按物理容量放宽、自动 GC 关闭，约 5.2 GiB 空闲低于 6 GiB 启动线，未在本地编译 Rust。Rust gate 在官方 runner 执行。

## 功能验证与测量条件

16 组 native 覆盖 eager/cooperative、worker/readable、full/bridge，每组合两次；**32 次 H2、64 个 H3/QUIC 探针**通过。64 个损坏 raw 帧被统计，64 次非法应用报文未获回复并等到预设超时，另有 48 次合法 wire 控制成功。双栈、H1/HTTPS、TCP 半关闭、SNI、UDP、bypass、16 次实际 reload、队列状态、清理和二进制摘要一致性通过。既有 TCP 重试不变，通过不证明首试全成功。

Linux `6.8.0-1070-azure`，release/x86-64-v2，同一 Edge 二进制比较两个模式，均使用 worker wake、bridge 统计。独立运行 1/2 个代理 CPU 预算，每预算 68 场景：64 代理场景加 4 direct 校准。Nginx 1.28.0、OpenResty 1.27.1.2 使用固定官方源码和匹配 worker 数/亲和性。四轮 Williams 轮换，1/16 稳定流、64/1200 字节，每流预热 1,000 次；单流计时 40,000 次，16 流每流计时 10,000 次。包含预热共 **14,756,000** 次请求/响应成功。

外部 C origin 与 C 客户端各占独立 CPU。单 XSK queue、IPv4、veth SKB/copy，相同 offload 设置严格读回；全部编译结束后才计时。客户端逐条校验完整正文、序号和长度，无 UDP 重试；origin 的不同源端口数必须等于流数，证明复用。Node 双向 redirect 精确为总请求两倍，流水线计数、所有错误/丢弃、配置、CPU 亲和性、时窗、每个 RTT、RPS、P99、清理与跨预算源码/二进制一致性均独立复核。

## 吞吐与尾延迟

RPS 为四轮绝对值中位数；变化为同轮 candidate/baseline−1 的四个比例中位数，因此不强求等于两个 RPS 中位数之比。正 RPS、负 P99 为改善。

| 代理 CPU | 流数 | 字节 | Eager RPS | Cooperative RPS | 配对 RPS 变化 | 配对 P99 变化 |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| 1 | 1 | 64 | 7241 | 7373 | +1.82% | -2.78% |
| 1 | 1 | 1200 | 7141 | 7145 | +0.49% | -1.58% |
| 1 | 16 | 64 | 48450 | 48453 | -0.27% | +2.27% |
| 1 | 16 | 1200 | 45357 | 46185 | +1.90% | -2.47% |
| 2 | 1 | 64 | 6164 | 6337 | +2.92% | -2.61% |
| 2 | 1 | 1200 | 6095 | 6197 | +1.71% | -1.21% |
| 2 | 16 | 64 | 60712 | 60664 | +0.21% | -0.17% |
| 2 | 16 | 1200 | 56506 | 56367 | -0.65% | -0.32% |

四轮不是置信区间。单轮最差 RPS 回退 −2.41%，P99 上升 +7.08%，均保留；不以最好一次替代完整分布。未出现报文丢失、损坏或队列故障，但候选并非每档都更快，故不更改默认行为。

同场景基线 RPS 中位数及 Cooperative 的同轮相对差距：

| 代理 CPU | 流数 | 字节 | Nginx RPS | OpenResty RPS | 相对 Nginx | 相对 OpenResty |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| 1 | 1 | 64 | 16561 | 16506 | -55.51% | -55.33% |
| 1 | 1 | 1200 | 16338 | 16305 | -56.33% | -56.05% |
| 1 | 16 | 64 | 62563 | 61703 | -22.59% | -21.35% |
| 1 | 16 | 1200 | 61157 | 61201 | -24.55% | -24.58% |
| 2 | 1 | 64 | 16444 | 16428 | -61.42% | -61.48% |
| 2 | 1 | 1200 | 16329 | 16335 | -62.16% | -62.09% |
| 2 | 16 | 64 | 90962 | 86127 | -33.39% | -29.56% |
| 2 | 16 | 1200 | 80012 | 86104 | -29.27% | -34.53% |

31/32 个基线 16 流场景客户端 CPU 达到 90%；另四个 direct 16 流校准的客户端和 origin 均达 90%。这些是保留的质量标记，不能据此声称测到了基线最大容量。全部观察 CPU steal 为 0，无小于 0.5 秒样本，也无快照窗口开销超过 5% 的标记。所有轮次和标记均保留，没有筛样本。

## 对架构差距的含义

原生代理进程树 CPU 快照除以成功计时 RTT，单流下 Eager 约 115.75–116.25 μs/RTT（1 CPU）、180.50–182.00 μs（2 CPU）；Cooperative 约 110.88–112.25 μs、174.38–175.88 μs，基线约 31–32 μs。它包含后台工作与快照边界，窗口宽于客户端计时，且不覆盖记在其他任务上的内核网络工作；这是近似进程 CPU 成本，不是函数级 CPU 归因或整机总成本。

双核单流的 CPU 成本更高、吞吐更低，与队列和 proxy runtime 之间的跨任务交接值得继续优化的判断一致。仅调整 idle 后让出并未消除差距。不能把第 12 轮 ptrace syscall 数直接换算成本轮时延比例，也不能将全部 write 归为日志/eventfd、全部 poll 归为阻塞 worker。

下一阶段已从源码确认 AF_XDP UDP relay 仍复制已有所有权的 Bytes，并为每会话分配 65,535 字节接收 scratch；拟用所有权传递减少复制，同时保留 PMTU、背压、错误、CID、统计和内核路径。这是下一轮候选，不包含于本报告。每会话省去缓冲分配不等于同量 RSS 下降，仍需实测。

## 证据、未解决与降级边界

[136 场景独立验证](evidence/af-xdp-idle-yield-2026-11-03/udp-validation.json)、[native 验证](evidence/af-xdp-idle-yield-2026-11-03/native-validation.json)、[调度反例](evidence/af-xdp-idle-yield-2026-11-03/helper-validation.json)、[全部配对/CPU/质量数据](evidence/af-xdp-idle-yield-2026-11-03/cpu-pair-quality.json)、[1 CPU 摘要](evidence/af-xdp-idle-yield-2026-11-03/cpu1-summary.json)、[2 CPU 摘要](evidence/af-xdp-idle-yield-2026-11-03/cpu2-summary.json)、[job/step](evidence/af-xdp-idle-yield-2026-11-03/final-jobs.json)。独立脚本、条件、所有 records 和逐文件摘要一并保留。

- UDP [原始 ZIP](evidence/af-xdp-idle-yield-2026-11-03/udp-raw.zip)，artifact `13212652932`，38,146,992 字节、2,202 文件，SHA256 `1f5efd42f6026a9c53675f0092a011475d8286db4f82e5bb7fcf683d9af75c92`。
- Native [ZIP](evidence/af-xdp-idle-yield-2026-11-03/native-raw.zip)，artifact `13211640070`，69,003 字节、37 文件，SHA256 `749e029362039816f4db813c9cfc21dbc704da506404ca86e7b8fd4503600a75`。
- Helper [ZIP](evidence/af-xdp-idle-yield-2026-11-03/helper-raw.zip)，artifact `13209284565`，3,550 字节，SHA256 `d08f271f0c43d4e5ef73cefe19eccc4a29daf62db79bf8758f17b50c9cd759ef`。

仍未解决整体性能差距；本轮没有 HTTP/H3/缓存容量、真实 NIC、零复制或长稳结论。已有故障/退役 RST teardown 的 `send_raw_frame(...).unwrap_or(false)` 会丢失发送错误原因，本轮未扩展该行为；既有 pcap 观察与功能 fixture 的部分 best-effort offload 边界沿用，计时 offload 则严格核验。

没有删除功能或实施功能降级，没有 H3 库切换、主分支合并、发行/部署，RKE 暂缓。默认 eager、worker 和 bridge 统计保持。缓存扩容不改变编译并发、磁盘余量或测量条件。
