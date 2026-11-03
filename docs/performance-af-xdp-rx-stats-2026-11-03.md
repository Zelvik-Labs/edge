# 第 10 轮：AF_XDP 接收统计与 UDP 对照

Bridge 接收路径不再为了未使用的临时统计执行完整 L4 分类。公开原始收包接口继续提供真实解析计数；实际代理的报文校验、准入、PMTU、队列处理和错误传播保留。136/136 个场景、1,360 万 RTT 独立复核通过，但八档吞吐变化中位数仅为 **-1.12%～+1.81%**，不是大的性能突破，仍未超过 Nginx/OpenResty。

被测源码为 `07021fe84f976795f28771c255243b6f6f7c21ee`，独立分支 `perf/rx-stats-20261103`，[Actions 41571736445](https://github.com/Zelvik-Labs/edge/actions/runs/41571736445) 两个执行 job 均成功，包含 Rust 单元/集成、Pingora、eBPF、全目标 check/Clippy、八轮原生套件与两套 UDP 对照。未选择的独立组件 job 按条件跳过，不计为本轮组件验证。

## 实现与对照含义

旧 bridge 调用 `poll_raw_once`，该函数在生成拥有所有权的原始帧时，还执行 `parse_l4_packet` 来更新临时 `parsed/parse_errors`。Bridge 不消费这些字段，后续代理仍会执行实际解析。新增私有 `poll_bridge_once` 与只包含包数、refill 和真实空 RX 观察的 `AfXdpRxObservation`，不填充虚假的解析成功或错误零值。

两个入口共享借用帧的 `poll_rx_frames`：保留 CQ 回收、fill、驱动 poll、可选 pcap、RX 归还及 refill；仍在归还 RX 所有权前复制原始帧。没有去掉这个必要的原始帧副本，也没有修改 `src/xdp/af_xdp/parser.rs`。

验证 CLI 新增 `--rx-stats full|bridge`，只用于 `proxy-smoke/proxy-reload-smoke`，不增加生产配置字段。Full 是同一新二进制里执行额外分类的控制，不是历史旧二进制逐字节相同的控制。LLVM 可以消除无用工作；不使用 `black_box` 强迫基线付出未必存在的开销。空 TCP 正文原本就未必需要堆分配，不能宣称每包必省一次分配。

## 本地与原生验证

本地 **108 项 AF_XDP 测试通过，0 失败、0 忽略**，全目标 Clippy 通过；3 项 wire builder 检查和 5 项分析工具检查通过。另核对非零 queue CPU、1/2 个 Nginx worker、Bash/Python/YAML 与嵌入 Python 语法。没有把本地不支持 AF_XDP 的环境当作原生成功证据。

CI 八轮按 full/bridge、worker/readable 两种组合各执行两次：`full-1-worker, bridge-1-worker, bridge-1-readable, full-1-readable, full-2-readable, bridge-2-readable, bridge-2-worker, full-2-worker`。独立解析原始产物确认：

- **16 次 H2、32 个 H3/QUIC 探针**成功，IPv4/IPv6、H1、HTTPS、TCP 半关闭、SNI、UDP 和 bypass/detach 检查通过。
- 公开 raw 统计实际记录 **32 个损坏帧**。应用层 **32 次非法报文探测均等到预设超时、未收到回复**；另有 **24 次合法报文**得到完整正确回复，包含合法的 IPv4 UDP 零校验和。非法 IPv4 头、UDP4/UDP6 校验和与非法 IPv6 UDP 零校验和分别检查。
- 每轮后端计数为 HTTP 2、HTTPS 4、TCP 2、UDP 5、H3 2、SNI 2、QUIC 2。UDP 5 包含原有两次与三次合法 wire 控制。
- 所报队列故障、丢弃、环满、无效描述符、准入/AQM 丢弃和残留 TX 均为 0。非法 raw 帧的预期解析错误与队列故障分开核对。
- 八次真实 reload 都替换 manager，supervisor 保持存活，前后 ready、二进制摘要和网络清理一致。退出仅剩私有 lo，无命名 netns 残留。

合法控制验证正文、来源地址和端口；非法应用控制只接受 socket 接收超时，其他错误直接失败，没有增加重试。既有 TCP 功能探针仍有重试，因此通过不证明每次首次连接都成功。该套件为 veth SKB/copy 功能验证，不证明真实 NIC、零复制或高负载下的损坏报文处理能力。

## UDP 测量条件

Linux `6.8.0-1070-azure` 上分别运行 **1 个、2 个代理 CPU** 两套矩阵，每套 68 场景：64 个代理场景加 4 个 direct 校准。Full/Bridge 均使用 worker wake，对照官方固定版本 Nginx 1.28.0、OpenResty 1.27.1.2；4 轮 Williams 轮换，1/16 条稳定 UDP 流，64/1200 字节正文，每流预热 1,000 次，单流计时 40,000 次、16 流每流计时 10,000 次。实际重算 **136 场景、1,360 万计时 RTT**，包含预热共 14,756,000 次请求/响应，全部成功。

四个 CPU 分别用于代理预算、客户端与独立 netns 回源；单核预算保留另一个代理 CPU 空闲。显式设置 XSK queue CPU，Nginx/OpenResty 的 worker 数和亲和性匹配预算，检查所有观察到的线程亲和性。单 XSK 队列、IPv4、SKB/copy，关闭并读回相同网络 offload。全部编译结束后才计时，检测到编译进程则失败。

固定 C 客户端逐条校验正文、序号和完整响应，不重试 UDP；回源实际不同源端口数必须等于并发流数，证明复用。双向 XSK redirect 必须精确等于请求总数的两倍；独立复核保留的流水线计数、错误/丢弃、实际配置、计时范围、RTT、RPS 和 CPU 输出。所有失败、离群值和质量标志保留，不能筛掉回退轮次。该条件与第 8 轮的单流迭代数及 CPU 预算不同，不直接计算跨轮升级百分比。

## 结果

下表 RPS 是各目标四轮中位数；最后两列是同轮 Bridge/Full 的变化中位数，不能直接由两列中位数相除重建。正 RPS、负 P99 为改善。

| 代理 CPU | 流/字节 | Full RPS | Bridge RPS | Nginx RPS | OpenResty RPS | RPS 变化 | P99 变化 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 / 64 | 7,003.3 | 7,028.8 | 15,946.3 | 15,898.9 | +0.41% | -1.37% |
| 1 | 1 / 1200 | 6,884.3 | 6,886.4 | 15,752.7 | 15,867.0 | +0.44% | +1.26% |
| 1 | 16 / 64 | 47,230.3 | 46,875.5 | 60,985.6 | 60,978.2 | -1.12% | +1.05% |
| 1 | 16 / 1200 | 44,188.3 | 44,118.2 | 59,269.6 | 58,705.2 | -0.09% | +1.26% |
| 2 | 1 / 64 | 5,975.9 | 6,032.1 | 15,888.2 | 15,949.0 | +1.67% | -7.33% |
| 2 | 1 / 1200 | 5,960.7 | 5,965.0 | 15,641.9 | 15,670.6 | +0.43% | -0.76% |
| 2 | 16 / 64 | 55,884.9 | 55,903.7 | 82,823.3 | 89,803.6 | +0.06% | +1.86% |
| 2 | 16 / 1200 | 52,280.2 | 53,150.0 | 88,488.4 | 83,460.5 | +1.81% | -1.50% |

双核 16 流大包的四轮 RPS 分别 +2.17%、+1.81%、+1.30%、+1.82%；单核 16 流小包为 -1.83%、-0.66%、+0.80%、-1.58%，未隐藏回退。双核单流小包、单流大包、16 流小包各有一轮 P99 回退 **+39.06%、+35.22%、+40.04%**，原始值全部保留。四轮只描述离散程度，不提供置信区间或“统计显著”结论。

保留私有 Bridge 入口及公开 Full 统计，不将这次删去未使用分类列为显著吞吐提升。各档变化方向并不一致，尤其单核 16 流仍需复测；这一结果不支持将主要差距归给临时解析/分配。Full 控制的可观测工作还受编译器优化影响，不能据此定量计算历史每包分配次数。

Bridge 相比 Nginx/OpenResty 的同轮吞吐变化中位数，单流仍约 **-55.88%～-62.10%**，16 流约 **-22.95%～-39.70%**。双核单流的观察值反而较低，16 流较高；两套预算先后运行，不能把预算间差异纯归因于核心数量，也不能据此直接确定跨核调度就是根因。进程树、系统调用和队列阶段的单独 profiling 仍需推进。

独立校验还确认每个场景实际回源源端口数等于流数，Node 双向 redirect 精确为请求总数的两倍，六个收发流水线计数相符；会话创建数匹配流数，所报队列、限流、准入、路由、发送错误及丢弃为 0。两预算间源码与四种二进制摘要完全相同。

所有 136 个样本观测到的 CPU steal 为 0，无低于 0.5 秒的短样本，CPU 快照窗口最大额外开销约 **0.355%**。代理样本最短约 1.65 秒，direct 最短约 1.21 秒。Nginx/OpenResty 的 32 个 16 流场景中 **31 个客户端达到 90% CPU**，因此仍不能当作这些基线的最大容量；Edge 客户端最高约 59.7%，未触发这一标志。单核 Edge 进程树最高约 99.8%，双核最高约 155.5%，均以一个核心为 100%；部分内核工作未记入这些进程，不从该数字单独推断可用余量。Edge 空闲 CPU 最高约 1.37%，基线报告值为 0；原生功能套件墙钟时间不作为吞吐数据。

## 证据

原生 CI 产物 `13203150279` 为 **35,245 字节、21 文件**，SHA256 **`6abedb9518db8539cf4faf997f391b35ef4e93e77f1c6863faa396ea4684f311`**，与 GitHub digest 一致。节点二进制 SHA256 为 `fc11739c3e00eccd7b01349580de68f6cc0531bd1cb18b43d70c0bff17861bfb`，H3 探针为 `ea52d93abcdd32b1165786bb67f3bcc0c160dba55e7240a58570232f13cd0beb`，八轮前后不变。

- [本地验证原始记录](evidence/af-xdp-rx-stats-2026-11-03/local-validation.zip)、[清单](evidence/af-xdp-rx-stats-2026-11-03/local-manifest.json)。
- [原生原始产物](evidence/af-xdp-rx-stats-2026-11-03/native-raw.zip)、[清单](evidence/af-xdp-rx-stats-2026-11-03/native-manifest.json)、[GitHub 元数据](evidence/af-xdp-rx-stats-2026-11-03/native-artifact.json)。
- [原生独立校验](evidence/af-xdp-rx-stats-2026-11-03/native-validation.json)、[提取报告](evidence/af-xdp-rx-stats-2026-11-03/native-reports.json)、[可重跑校验器](evidence/af-xdp-rx-stats-2026-11-03/validate-native.py)。`native-job-step-snapshot.json` 只是上传时的中途状态，不能代替最终 job 记录。
- [最终 job/step 记录](evidence/af-xdp-rx-stats-2026-11-03/final-jobs.json)。
- [UDP 原始产物](evidence/af-xdp-rx-stats-2026-11-03/udp-raw.zip)、[逐文件摘要](evidence/af-xdp-rx-stats-2026-11-03/udp-manifest.json)、[GitHub 元数据](evidence/af-xdp-rx-stats-2026-11-03/udp-artifact.json)。产物 `13203723870` 为 **38,504,122 字节、2,202 文件**，SHA256 **`55ec16a3f14eafe8ffd5d04c2fb017d8327e6f9f73855121e40007de4c5cf2d5`** 与 GitHub digest 一致。
- [UDP 独立重算](evidence/af-xdp-rx-stats-2026-11-03/udp-validation.json)、[可重跑校验器](evidence/af-xdp-rx-stats-2026-11-03/validate-udp.py)、[原始运行汇总](evidence/af-xdp-rx-stats-2026-11-03/udp-summaries.json)、[条件/清理](evidence/af-xdp-rx-stats-2026-11-03/udp-conditions.json)、[CPU 与质量标志](evidence/af-xdp-rx-stats-2026-11-03/udp-diagnostics.json)。正文/序号由固定 C 客户端在运行时检查，RTT 本身不能重建正文；重算保留输出不能替代这项运行时校验。

## 未解决、设计限制与功能降级

可选 pcap 既有 open/write 错误仍被忽略，本轮未改变这些函数，也不将抓包成功当作证据。既有功能套件的部分 offload 设置仍为 best effort；计时套件单独严格检查实际设置。零健康队列目前仍是受控 manager 状态回归，启动 ready 早于 wake 初始化的窗口仍待验证。

UDP 结果不代表 H3/HTTP/缓存吞吐，不证明整个 Edge 超过 Nginx/OpenResty。CPU 进程树统计包含快照开销，部分网络内核工作可能记在其他任务；客户端接近满核、direct 接近上限或短样本会影响容量判断。没有真实硬件或长稳结论。

没有申请或实施功能降级，没有切换 H3 库、发布、部署或合并 main；默认 wake 仍为 worker。RKE 暂缓。云缓存按物理磁盘容量放宽且关闭自动 GC，保留编译并发和启动 6GiB / 运行 5GiB 空间底线。

为保留编译缓存和原始证据，将旧轮次的一个 perf 样本及九个闲置可执行文件无损 gzip 归档，逐个解压回算 SHA256，回收 **385,013,786 字节**。旧 root 私有 perf 文件因当前身份无读取权限而未改动，未绕过权限；[清单和恢复命令](evidence/af-xdp-rx-stats-2026-11-03/storage-compression.json)保留原字节摘要、模式和时间。不是例行缓存清理。

下一轮已从源码定位：每个客户端 UDP 数据报的 H3/QUIC 端口判断构造完整端口集合，可能复制策略和服务器列表；站点标记检查还复制小写字符串。候选在单一配置读锁内做点查询并借用 ASCII 后缀，保留策略优先级、范围宽容解析和 H3 优先规则，按现有完整集合 API 做语义对照。这是下一轮候选，**不包含在本报告的被测提交和吞吐中**。站点端口回退仍可能是 O(站点数)，后续须同时测量组件成本、配置更新和整机表现，不能仅凭源码宣称消除主要差距。
