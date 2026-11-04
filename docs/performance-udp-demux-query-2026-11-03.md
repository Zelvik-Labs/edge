# 第 11 轮：UDP 解复用端口查询

将 AF_XDP 每个客户端 UDP 数据报的完整端口集合构造改为一次配置快照中的点查询，并消除站点标记检查的小写字符串副本。固定提交的 CI、八轮原生协议和 32 个 release 组件场景通过；960 个原始样本独立重算，观测到的 CPU steal 为 0。**这是配置查询的改善，尚未测量本改动的整机吞吐收益，也未证明超过 Nginx/OpenResty。**

源码 `1fbca822e49c3242edf89c7fd1a134de81ab3b27`，独立分支 `perf/udp-port-query-20261103`，[Actions 41579056235](https://github.com/Zelvik-Labs/edge/actions/runs/41579056235) 两个执行 job 均成功：全目标 check/Clippy、Rust 单元/集成、Pingora、eBPF、八轮真实节点检查与 release 组件基准。未选择的独立探针 job 按条件跳过。

## 已修复

`QuicUdpDemuxManager.receive_af_xdp_datagram` 原先调用 H3 完整监听集合，再构造 QUIC 转发端口集合，只为判断当前一个端口。前者复制含字符串的策略；站点回退还复制服务器 `Arc` 列表，展开端口范围并构造集合。普通 UDP 也经过这条路径，开销并不来自 Quinn 或 tokio-quiche 本身。

新增 `ConfigStore.af_xdp_demux_http3_enabled_sync`，一次读锁内借用策略、服务器和现有 QUIC 索引。H3 命中返回 `Some(true)`，否则 QUIC 端口返回 `Some(false)`，否则 `None`。生产 main、smoke 和测试的构造均核对为共享同一个 ConfigStore。原有完整集合 API 继续供监听器使用，没有 TTL 缓存或新的派生索引。

保留原配置语义：策略优先级为 cluster ID → 0 → 现有任一 map 值，不排序；有效正 u16 全局端口只匹配该端口；0、负数、越界仍按站点回退。保留 HTTPS/H3 启用、SNI/QUIC 排除、旧逻辑不检查 `server.is_on` 的行为，以及反向范围、冒号、非法输入和 0 的宽容解析。QUIC 索引值为 `None` 表示路由模糊，端口仍须进入 demux。H3 优先规则不变。

`ServerConfig` 的 `@quic` / `@sni_passthrough` 检查改为借用字节后缀并按 ASCII 忽略大小写，保留 Unicode 前缀、主名称/子名称、尾空白和标记位置语义。没有修改 Retry/ALPN、域名绑定、QUIC 路由、内存/压力/准入预算、正文、统计或错误传播。

## 正确性验证

本地 **157 个不同测试通过，0 失败、0 忽略**：131 个配置相关、26 个 QUIC demux。此前单独执行的 3 个核心回归与这些测试重叠，不重复相加。全目标 Clippy 与修改后的 benchmark 目标 Clippy、Rustfmt、YAML/Python/嵌入 Python 语法均通过。

新查询在 14 组稳定配置下逐一对照全部 65,536 个 u16 端口，共 917,504 次断言；对照调用保留的生产监听集合 API。覆盖缺失/关闭策略、负数/零/有效/越界端口、H3 与 QUIC 重叠、关闭站点或 HTTPS、范围宽容解析、实际替换/清除服务器。另测策略优先级、模糊 QUIC，以及 392 个名称/标记组合。20,000 次配置写入与 20,000 次点查询并发时，端口始终属于一个已安装 epoch；未声称复现生产重载丢包。

Linux `6.8.0-1070-azure` 的八轮 full/bridge × worker/readable 验证独立复核通过：**16 次 H2、32 个 H3/QUIC 探针**，IPv4/IPv6、H1、HTTPS、TCP 半关闭、SNI、UDP、raw redirect/bypass、detach 与实际 manager reload 均成功。32 个损坏 raw 帧实际计入公开统计；32 次非法应用探测未收到回复，24 次合法 wire 控制收到完整回复，包含合法 IPv4 UDP 零校验和。

每轮后端计数 HTTP 2、HTTPS 4、TCP 2、UDP 5、H3 2、SNI 2、QUIC 2；所报队列故障、丢弃、环满、无效描述符、准入/AQM 丢弃和残留 TX 为 0。manager 实际替换，supervisor 存活，二进制前后摘要一致，退出仅余私有 lo，无命名 netns 残留。原生套件的 H3 全局端口为 8443；大量站点回退的网络负载、策略切换下的原生满载重载尚未验证。既有 TCP 探针有重试，通过不证明首次连接都成功。

## 组件条件与结果

同一 release 二进制比较保留的 `Http3ProxyManager.desired_ports_sync` + `quic_passthrough_ports_sync` 与新点查询。1 / 1,000 个 H3 站点，另有一个 QUIC 转发站点；全局端口 8443 或按站点回退 0；查询 H3、QUIC 和普通 UDP 端口，共 **32 个函数，每函数 30 个 Criterion 样本**。H3 站点均监听 443–444，QUIC 站点监听 5300，路由结果在计时外明确断言。

固定在 CPU 0，可用 CPU 为 0–3；`target-cpu=x86-64-v2`、bench opt-level 3、debug assertions/overflow checks 关闭，使用 System allocator。全部编译结束后运行，源码和二进制摘要前后相同。总运行约 117.77 秒，四个 CPU 的整段 steal 均为 0。该观测不是逐函数 steal 采样，也不等于独占物理机器。

下表为从全部原始 `times / iters` 重算的**每次查询平均纳秒**，同时与 Criterion 均值、样本中位数和脚本汇总核对。负变化为改善。

| H3 站点 | 策略端口 | 查询端口 | 集合 ns | 点查询 ns | 变化 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 8443 | 443 | 182.84 | 51.21 | -71.99% |
| 1 | 8443 | 8443 | 112.07 | 30.90 | -72.43% |
| 1 | 8443 | 5300 | 181.66 | 51.62 | -71.59% |
| 1 | 8443 | 9999 | 180.54 | 51.12 | -71.69% |
| 1000 | 8443 | 443 | 181.70 | 51.07 | -71.89% |
| 1000 | 8443 | 8443 | 112.17 | 30.64 | -72.68% |
| 1000 | 8443 | 5300 | 180.61 | 52.39 | -70.99% |
| 1000 | 8443 | 9999 | 181.56 | 51.11 | -71.85% |
| 1 | 0 | 443 | 215.44 | 68.57 | -68.17% |
| 1 | 0 | 8443 | 281.66 | 84.68 | -69.94% |
| 1 | 0 | 5300 | 282.50 | 85.26 | -69.82% |
| 1 | 0 | 9999 | 281.21 | 85.06 | -69.75% |
| 1000 | 0 | 443 | 112,308.64 | 68.57 | -99.94% |
| 1000 | 0 | 8443 | 112,329.58 | 36,913.66 | -67.14% |
| 1000 | 0 | 5300 | 112,205.24 | 37,215.23 | -66.83% |
| 1000 | 0 | 9999 | 112,282.94 | 37,045.39 | -67.01% |

全局端口场景约为原查询的 3.45–3.66 倍速度，但省去的是本组件百纳秒级成本。1,000 站点回退且 443 命中时，新查询在**第一个站点**即结束；约 1,638 倍的比例仅属于这个明确的提前命中场景，不是 H3 或整个节点提速。三个回退未命中/QUIC 场景仍需扫描全部站点，从约 112μs 降至 37μs，仍有线性扩展成本。

所有原始样本、离群值和 Criterion estimates 保留。函数按集合/点查询先后测量，不是四轮整机轮换对照；控制也使用新的 ASCII 后缀实现，因此不能量化后缀优化的独立收益，也不是历史旧二进制逐字节对照。未测每包实际堆分配次数，不将源码中的集合消除换算为固定分配数量。

本地 dev opt-level 2 的同一 32 函数也跑通，960 样本独立重算通过；但所绑 CPU 整段 steal **27.13%**，不据此作定量性能结论。编译开始于上一 HEAD 加同源未提交 Rust 文件，随后发布为 1fb，可能嵌入父提交版本戳；实际源文件/二进制摘要与上下文已保留。上表只使用 CI 固定提交的 release 结果。

## 证据

- [最终 job/step](evidence/udp-demux-query-2026-11-03/final-jobs.json)、[本地原始验证与 dev 样本](evidence/udp-demux-query-2026-11-03/local-validation.zip)、[本地清单](evidence/udp-demux-query-2026-11-03/local-manifest.json)、[dev 质量标志](evidence/udp-demux-query-2026-11-03/dev-validation.json)。本地归档 278 文件、211,263 字节，SHA256 `27926dad682ce976d309d833004d01afe9659bdd96bc01d0e77851c8d683cc25`。
- [原生原始产物](evidence/udp-demux-query-2026-11-03/native-raw.zip)、[逐文件摘要](evidence/udp-demux-query-2026-11-03/native-manifest.json)、[GitHub 元数据](evidence/udp-demux-query-2026-11-03/native-artifact.json)、[独立验证](evidence/udp-demux-query-2026-11-03/native-validation.json)、[提取报告](evidence/udp-demux-query-2026-11-03/native-reports.json)、[校验器](evidence/udp-demux-query-2026-11-03/validate-native.py)。产物 `13206166452` 为 35,234 字节、21 文件，SHA256 `341492ebbf1768004a0a86d4474fd4abc823249ef31668946f317bd69254cd4c`。节点摘要 `fc8e61fbb5505d48a69e1ebccb1b1729f41a6895d24d9eb2a075e8dddd6f3396`，H3 探针 `ea52d93abcdd32b1165786bb67f3bcc0c160dba55e7240a58570232f13cd0beb`。
- [release 组件原始产物](evidence/udp-demux-query-2026-11-03/component-raw.zip)、[逐文件摘要](evidence/udp-demux-query-2026-11-03/component-manifest.json)、[GitHub 元数据](evidence/udp-demux-query-2026-11-03/component-artifact.json)、[独立重算](evidence/udp-demux-query-2026-11-03/component-validation.json)、[校验器](evidence/udp-demux-query-2026-11-03/validate-component.py)、[条件](evidence/udp-demux-query-2026-11-03/component-metadata.json)、[汇总](evidence/udp-demux-query-2026-11-03/component-summary.json)。产物 `13206359179` 为 183,783 字节、261 文件，SHA256 `2d9529f4f67b3a0150ccb7553ee1667af05abba29328a8e37ca2b434dd67fbdc`。基准二进制摘要 `c6c9139ec64a3e3d18390c0b9ceeeabf295b3d06cf9496ad5e67df919223ef23`。ZIP 均与 GitHub digest 一致。

## 仍未解决与设计限制

站点回退仍为 O(站点数)，并在配置读锁内扫描，配置更新等待尚未单独测量。预建端口索引已有源码与组件数据依据，但需要覆盖全部配置更新路径、原子发布和匹配整机负载后再作结论。

第 10 轮全局端口配置下的整机差距和 P99 回退仍未解决。本轮没有新的 Nginx/OpenResty 吞吐、HTTP 缓存或硬件容量结论。下一轮独立系统调用诊断已提交，使用预热后的真实追踪；受 ptrace 扰动的时间和累计 syscall 等待时间不能当作原生容量或 CPU 消耗。

本地宿主不支持原生 AF_XDP，网络验证来自 CI veth SKB/copy。真实 NIC、零复制、硬件 NEED_WAKEUP 和长期稳定性仍待验证。既有可选 pcap 忽略 open/write 错误及 ready 早于 wake 初始化窗口没有在本轮修复。

## 功能降级与资源

没有申请或实施功能降级，没有替换 H3 库、发布、部署或合并 main；RKE 暂缓。一次后续测试因低于 6GiB 启动线被拒绝，未运行测试；无损归档 68 份旧日志、逐份解压核对 SHA 后回收 289,347,365 字节，随后 26 项 QUIC 测试实际通过。误用 wrapper 的 `bench` 子命令被用法检查拒绝，改为受管 `build --bench`，原日志全部保留。没有把这两次工具启动拒绝写成 Rust 测试失败或绕开 mbx；编译缓存、cloud 预算和 6/5GiB 空间保护保留。
