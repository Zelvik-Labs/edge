# 第 18 轮：WAF 有界状态表的固定预分配成本

**15/15 个独立进程测量和生产表现有 7 项测试通过**。代表容量 1,677,721 条时，7 张空表的 RSS 增量中位数为 **448.016 MiB**，构造 **229.126 ms**，空表完整遍历/retain 分别 **24.566/24.529 ms**。这是实际生产表源码的组件数据，说明空表预分配是明确的优化目标；尚未修改生产分配方式，也不把它当作整个 Edge 的精确内存归因。

被测提交 `9239da7ccf39cd31dc5712fef5829e692d36a326`，分支 `perf/waf-state-reservation-20261103`，[Actions 41613464184](https://github.com/Zelvik-Labs/edge/actions/runs/41613464184) 成功。独立 crate 用 `#[path]` 包含实际 `src/firewall/bounded_map.rs`，该文件以及调用方、governor 与 `708785c` 字节相同。没有改写或复制简化版算法来替代测量。

## 分配来源与条件

`WafStateManager::new` 构造 blocks、kernel_blocks、list_blocks、whitelists、list_whitelists、graylists、list_graylists 共 7 张表。每表 64 个 segment，每 segment 在构造时执行 `vec![None; seg_cap]`。容量来自 governor 的状态预算，并在本次构造时确定；本探针使用三档代表容量，不冒充网络 runner 上实际解析的 governor 值。

工具链 Rust 1.99.1、release 默认优化 3、x86-64-v2，Linux `6.8.0-1070-azure`，CPU 0 继承亲和性，所有编译完成后运行。独立 workspace 未继承完整 Edge 的 fat LTO/codegen-units=1/panic=abort 配置。与 Edge 相同版本的 31 个 registry 依赖均锁定；本 crate 使用 **System 分配器，Edge 使用 mimalloc**。读取实际页大小 4,096 字节。15 个新进程，每容量 5 轮，交替正/反容量次序；所有记录保留，观察 CPU steal 全部为 0。7 项现有生产测试、Clippy、release 均通过，本地只做格式/脚本检查及离线 lockfile 元数据生成，没有本地 Rust 编译。

实测 `Option<((i64, IpAddr), i64)>` 为 **40 字节、8 字节对齐**。当前容量计算为 `max(floor(requested/64),64)*64`，故代表容量 1,677,721 实际有 1,677,696 槽；报告保留该向下取整行为，没有把注释中的“至少 requested”当作代码事实。

## 全部容量结果

下表为五轮中位数。RSS 为同一新进程中构造后的驻留量减去构造前驻留量；扫描覆盖七张表，查询每组共 200,000 次并核对结果。

| 请求容量/表 | 实际槽/表 | 空表 RSS 增量 MiB | 构造 ms | 空表遍历 ms | 空表 retain ms | miss ns/次 | hit ns/次 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 65,536 | 65,536 | 17.523 | 9.247 | 0.855 | 0.546 | 21.90 | 11.89 |
| 1,677,721 | 1,677,696 | 448.016 | 229.126 | 24.566 | 24.529 | 21.74 | 11.90 |
| 8,000,000 | 8,000,000 | 2136.766 | 1095.846 | 114.341 | 115.710 | 22.08 | 11.89 |

七表 slot payload 分别为 17.500、447.993、2,136.230 MiB；RSS 增量略大，不能把额外字节都解释为业务数据。大容量档构造约 1.096 秒，即使没有任何记录也增加约 2.087 GiB 驻留内存。RSS 是进程驻留量，不是私有物理内存或分配器精确账本。

每表第一次写入的 35 个样本在每档的中位数均为 60 ns，全档范围 50～1,933 ns。它在空表查询之后执行，目标 probe 区域已访问，且整个 slot 数组已构造；不能称为未分配内存的首次触页成本。任何按需分配候选必须重新测量这一步，不能把启动分配成本转移到首次写入后仍宣称没有代价。

固定八槽查询成本保持约 21.7～22.1 ns miss、11.9 ns hit；大表的构造和完整扫描成本明显随容量增加。这不能说明每个生产请求都执行全表扫描，也不把组件扫描时间直接换算为网络延迟。

## 下一步、未解决和边界

已完成可复核的预分配成本定位；空表大分配和扫描仍存在，尚未实施修复。优先评估有界的按需分块：保持相同 key、八槽探测/淘汰顺序、容量、过期、条件删除及快照语义，比较空表、稀疏/满表、首次写入和稳定查询。同时审计现有跨 segment 淘汰加锁及并发软上限；目前只有源码疑点，未把它们当作已实证或已修复的故障。

第 17 轮 Kernel/AF_XDP/Nginx/OpenResty 的网络矩阵使用另一 runner、另一运行提交，独立进行。本轮没有 Nginx/OpenResty 吞吐、H3 缓存、真实 NIC 或长稳结论，不能据此宣称整体性能提升或超过基线。System 与 mimalloc、代表容量与完整 governor、组件与全 Node 的限制都保留。没有功能删除、容量削减或经审批的降级；没有主分支合并、发行/部署，RKE 仍暂停。

## 证据

[独立核验和全部容量计算](evidence/waf-state-reservation-2026-11-03/validation.json)、[原始 records](evidence/waf-state-reservation-2026-11-03/raw/measurements/records.json)、[环境及源码摘要](evidence/waf-state-reservation-2026-11-03/raw/measurements/metadata.json)、[原始五轮 summary](evidence/waf-state-reservation-2026-11-03/raw/measurements/summary.json)、[job/step](evidence/waf-state-reservation-2026-11-03/final-jobs.json) 和独立脚本一并保留。7 项生产测试、Clippy、构建及每进程原始日志完整保留于 ZIP；未修改终端控制字符或原始换行。运行独立脚本前将 ZIP 解压到证据目录的 `extracted/`，以证据目录和输出 JSON 为两个参数。

[原始 ZIP](evidence/waf-state-reservation-2026-11-03/raw.zip)：artifact `13220660289`，19,779 字节，36 文件/70,546 解压字节，SHA256 `01257fc0aa399c335519e563f78cf65b80e8e846c12e9ece5a9287ded3249378`。每个归档条目、解压文件、raw stdout、摘要、中位数及源码 hash 均独立检查。可执行文件未包含在归档中，二进制 hash 来自测量者 metadata，没有宣称离线重算了该二进制摘要。
