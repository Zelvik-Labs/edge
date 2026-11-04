# 第 7 轮：AF_XDP Tokio 节点实验路径与错误事件

直接 Tokio 唤醒已接入完整节点的显式实验命令，**四轮 IPv4/IPv6 协议、重载和清理检查全部通过**。默认服务继续使用 owned poll worker；本轮验证了接入正确性，尚无节点吞吐提升结论。

最终源码 `4651bd39f576687240bbefe99bfee38927b96c3d` 已推送到 `perf/xdp-tokio-20261102`。[Actions 41553649436](https://github.com/Zelvik-Labs/edge/actions/runs/41553649436) 的 Check/Clippy、Test 两个执行 job 成功；未选择的独立组件 job 按配置跳过。实现始于 `68ecbffc98f54373e5abeab49283e54d3b9d243c`，测试排查延续至 2026-11-03。

第 6 轮的组件耗时下降 67.56% 属于当时 READABLE-only 基准，不能直接代表这里加入错误事件处理后的完整节点。RKE 仍暂停，原生结果来自 GitHub Actions 的隔离 veth。

## 已修复和实现

`AfXdpReadiness` 注册并拥有复制的 socket FD，没有伪造 `'static` 借用，也不改变原 socket 的文件状态标志。bridge 把 guard 保留到该 guard 获得之后的一次实际空 RX 观察；部分读取、成功读取、未执行读取和错误都不能清除 readiness。`rx_observed_empty` 明确区分成功返回但没有执行 RX 的情况。原有 CQ 回收、fill 补充、零超时 poll、驱动唤醒和 TX kick 保留。

新增真实 UDP 错误测试首先失败：向本地已关闭 UDP 端口发送后，底层 poll 报 `POLLERR=0x8`、`SO_ERROR=ECONNREFUSED`，但 Tokio 1.53.1 的 `readable()` 等待超时。源码确认它只等待 READABLE；ERROR 是独立 interest。候选随后改为同时注册和等待 `READABLE|ERROR`，并检查返回 guard 中的错误/关闭位。故障仍禁用对应队列 redirect，并走已有会话 abort/RST 退出流程，不转成空读或静默重试。

`xdp proxy-smoke` 和 `xdp proxy-reload-smoke` 增加 `--wake worker|readable`，供同一二进制对照；报告包含实际模式。`--kernel --wake readable` 明确拒绝。没有新增生产配置字段、改变兼容配置解析或替换 H3 库。

## 验证结果

最终本地完整库 **1,300 项通过、0 失败、2 项既有忽略**，全目标 Clippy 通过。独立唤醒包 13 项通过、0 忽略，含原 worker、Tokio 基础行为和新增的 FD 所有权/取消、部分排空、HUP、纯 ERROR、注册失败五项节点候选测试；其 Clippy 通过。两组测试有重叠，不合并为独立用例数量。修复前的 12 通过/1 失败日志以及后续隔离测试失败均保留。

手动 workflow 新增 `xdp_node_smoke=true`：构建相同节点和 H3 客户端二进制，再以 worker/readable/readable/worker 顺序跑四个独立功能套件。强制检查 HTTP/2 客户端能力；覆盖 IPv4/IPv6 TCP 半关闭、HTTP/1.1、HTTP/2、HTTPS、SNI、UDP、H3、QUIC、generation reload、raw redirect/bypass 和未匹配流量。每个失败独立保留，不能被后一次成功覆盖。

测试在不同于调用者的 netns 与 mount namespace 中运行，私有挂载 bpffs 和 `/run/netns`，配置使用临时目录；保留完整输出、退出码、清理结果和前后两次二进制摘要。没有编译与该主机上的测试并行。原始记录见 [本地 ZIP](evidence/af-xdp-node-readiness-2026-11-02/local-raw.zip)、[校验清单](evidence/af-xdp-node-readiness-2026-11-02/local-manifest.json)、[第一次 CI 失败](evidence/af-xdp-node-readiness-2026-11-02/ci-first-raw.zip)、[第二次 CI 失败](evidence/af-xdp-node-readiness-2026-11-02/ci-second-raw.zip)。入口见 [运行说明](../scripts/perf/afxdp-node/README.md)。

## 前三次 CI 失败记录

所有失败保留，未算作原生协议成功。

| 运行与源码 | 停止原因 | 原生套件与证据 |
| --- | --- | --- |
| [41547170519](https://github.com/Zelvik-Labs/edge/actions/runs/41547170519)，`68ecbff` | 全仓门禁通过，新增构建步骤仍请求小写 `zelvik-edge`；实际目标是 `Zelvik-Edge` | 未执行；[244 字节产物](evidence/af-xdp-node-readiness-2026-11-02/ci-first-raw.zip) |
| [41548207258](https://github.com/Zelvik-Labs/edge/actions/runs/41548207258)，`08cd461` | 全仓门禁通过；sudo PATH 无 Cargo，旧脚本在预编译模式仍无条件要求该工具 | 四次前置失败，清理成功、二进制一致；[5,224 字节/12 文件](evidence/af-xdp-node-readiness-2026-11-02/ci-second-raw.zip) |
| [41549959626](https://github.com/Zelvik-Labs/edge/actions/runs/41549959626)，`b05cc8a` | `critical_samples` 为 0、期望 3；锁中毒再引发 7 项失败。库测试 1,289 通过、8 失败、2 既有忽略 | 未执行；[完整 job 日志及元数据](evidence/af-xdp-node-readiness-2026-11-02/ci-third-failure.zip) |

`08cd461` 修正构建与查找的目标名，`b05cc8a` 仅在真正构建时要求 Cargo，并让失败 job 也保留 Rust 缓存；两次修复未改 Rust 源码。随后 `4651bd3` 修复共享测试状态，并固定实际执行路径与所记录哈希对应。

## CI 共享状态修复

`memory_shed` 的测试通过 registry 锁串行执行，但 fake-time Critical 样本与其他组件的真实压力观察共同修改全局原子状态；该锁不能隔离后者。将同一组十个原子字段归入 `ShedState`，生产仍使用一个静态实例，测试分别持有独立实例，保留 registry 的原有取消/排空检查。新增两个状态并行观察的回归用例；没有降低线程数、放宽原断言或容忍锁中毒。

源码归一化检查确认五个有状态核心函数在替换状态引用后正文一致，阈值、原子顺序、回收证据和排空逻辑保留。该修复是状态隔离与验证稳定性工作，不作为性能收益。本地完整库测试 1,300 通过、0 失败、2 项既有忽略，全目标 Clippy 通过；最终 CI 两个执行 job 成功，原生四轮均通过。

H3 NAT 用例单独运行通过，但完整库运行时返回 NoRoute。初次只注入 governor 快照仍不足以隔离：压力判断还合并全局 memory/utilization 滞回、L4 聚合和内核 SYN 防御状态。最终为核心提供完整观测来源：生产唯一来源是实时 governor 及全部原有压力组合，单元测试可提供有限预算与已计算压力。没有为测试清空全局防御状态，也没有将异常转成成功。高压测试准备阶段缺少路由窗口记录的问题也已修正，失败日志保留。

NAT 用例的 256 路由、队列满及计数断言保留，新增高压按 IP 限流和总路由上限检查。使用明确的来源类型，避免为不同测试闭包重复生成完整异步收包函数；固定观测变体仅编译到单元测试，生产不增加动态分配。源码复核确认实时压力组合和其余包处理逻辑保持原顺序。

修复提交 `4651bd39f576687240bbefe99bfee38927b96c3d` 已推送；第四次 [Actions 41553649436](https://github.com/Zelvik-Labs/edge/actions/runs/41553649436) 的 Check/Clippy 与 Test 两个执行 job 均成功；未选择的独立组件 job 按配置跳过。原生 wrapper 同时明确固定执行路径，确保执行文件与前后记录哈希一致。

## 真实节点原生结果

最终原生运行于 Linux **6.8.0-1070-azure**，使用同一对 `Zelvik-Edge` / `h3-probe` 二进制，四轮顺序与结果如下。

| 轮次 | 唤醒方式 | IPv4/IPv6 协议套件 | generation reload | 网络资源清理 |
| --- | --- | --- | --- | --- |
| 1-worker | owned worker | 通过 | 通过 | 通过 |
| 1-readable | READABLE + ERROR | 通过 | 通过 | 通过 |
| 2-readable | READABLE + ERROR | 通过 | 通过 | 通过 |
| 2-worker | owned worker | 通过 | 通过 | 通过 |

每轮覆盖 TCP 半关闭、HTTP/1.1、HTTPS、HTTP/2、SNI、UDP、H3、QUIC，另有 raw redirect/bypass、protect 未匹配流量与 detach 检查。独立复核了 **8 次 H2 检查和 16 个 H3/QUIC 探针成功输出**，没有跳过 H2。每轮应用后端均记录 HTTP 2、HTTPS 4、TCP 2、UDP 2、H3 2、SNI 2、QUIC 2 次请求或连接。四轮的队列故障、RX 丢弃/环满/无效描述符、TX 无效描述符/残留 in-flight、准入拒绝、拥塞丢弃和 AQM 丢弃计数均为 0。这里是轻量功能检查，不能外推成高负载零丢包。

四次 reload 均实际替换 manager，bridge supervisor 保持存活，前后 ready 条件通过。每次退出后只剩私有命名空间的 lo，无残留命名 netns；测试前后二进制摘要一致。

产物 `13194725867`：**17,964 字节、12 文件**，SHA256 `184cfa12641f2aa2246eab7468adcade32704819dc491c8d540fbfad1a014958` 与 GitHub 元数据一致。完整产物见 [原始 ZIP](evidence/af-xdp-node-readiness-2026-11-02/ci-final-raw.zip)、[逐文件摘要](evidence/af-xdp-node-readiness-2026-11-02/ci-final-manifest.json)、[提取的协议/重载报告](evidence/af-xdp-node-readiness-2026-11-02/ci-final-reports.json)、[独立复核](evidence/af-xdp-node-readiness-2026-11-02/ci-final-validation.json)、[job 与 step](evidence/af-xdp-node-readiness-2026-11-02/ci-final-jobs.json)。第三次 CI 的完整失败日志见 [原始归档](evidence/af-xdp-node-readiness-2026-11-02/ci-third-failure.zip)。

## 设计限制与后续

当前云宿主不支持 AF_XDP socket，完整原生路径依赖 GitHub Actions；不能把本地单元测试算成原生协议通过。veth 使用显式 SKB/copy，真实 NIC 的零复制、RX NEED_WAKEUP 和长期稳定性仍未验证，RKE 继续暂停。既有 smoke 中 TCP 有重试，因此即使最终套件通过，也不等于每次首次尝试都成功。功能套件墙钟时间只用于定位卡顿，不作为吞吐或尾延迟成绩。

下一步为包含错误事件的集成候选补齐匹配性能对照，并补充启动失败/零存活队列的故障注入和更长重载压力测试。纯 H3 端口升压后重新走 ClientHello 解析，也需要在保留限流与资源边界的条件下单独分析；当前功能套件的 H3/QUIC 共享端口不能代替纯 H3 端口压力对照。另发现 RX 路径为了统计 `parsed/parse_errors` 调用会构造 owned payload 的完整解析器，之后 bridge 再做校验与拷贝；后续可用借用视图减少重复分配和解析，但必须保留校验和、畸形包、分片、IPv6 扩展头、VLAN、ICMP/PMTU 和统计语义。目前仅为代码发现，尚无这部分收益数据。

## 功能降级

没有申请或实施功能降级。默认节点后端和生产配置保持当前行为；实验路径中的错误具有明确故障状态。
