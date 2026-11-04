# io_uring 工作队列限额与 CPU 范围（2026-11-02，第 4 轮）

本轮从 `fa2c2cbc429ff41a61039103db2cd622a5ea76be` 开始，在独立分支 `perf/io-wq-bounded-20261102` 继续默认关闭的 I/O 库实验。实际节点的 TCP/SNI 仍使用第 1 轮 readiness splice，本轮不改变代理选择或生产配置。

## 问题与实现

第 3 轮 GitHub runner 的 18 个 io_uring 正文场景中，结束快照观察到 8～175 个 `iou-wrk`，默认候选没有设置内核工作线程上限。另核对 Linux [v6.8 io-wq.c](https://github.com/torvalds/linux/blob/v6.8/io_uring/io-wq.c)：初始化的 CPU 掩码来自 `cpu_possible_mask`，不能仅靠用户态进程的 `sched_setaffinity` 假定内核线程也在同一范围。上一轮没有记录每个线程的有效 CPU 掩码，已在原报告补充这一限制，原始样本保持。

候选在 issuer 线程创建 ring 后、接入连接前，读取该线程的 CPU affinity 并注册 `IORING_REGISTER_IOWQ_AFF`。默认实验配置另注册 bounded/unbounded 两类工作线程上限 `[2, 2]`，显式 `None` 仅供保留内核默认数量的对照。配置拒绝 0 和超出内核有符号整数范围的数值：注册 API 的 0 表示“保持现状”，不能把配置 0 当成关闭工作线程。系统调用错误显式返回，没有退回不受限制的队列。

限额在连接和取消请求排空后、issuer 退出前读回，并与 CPU 数一起输出。这里不能在首次 I/O 前把返回的零当成有效上限；Linux v6.8 的注册实现会在尚未创建 io-wq 时返回零。新增观测不在正文热路径增加锁、分配或系统调用。

本轮的 `uring-unbounded` 保留内核工作线程数量默认值，但**同样显式绑定 CPU**；它与第 3 轮未注册 CPU 范围的 `uring` 不完全相同。两种候选使用同一二进制和状态机，区别仅在启动时是否注册 `[2,2]`。与 readiness 的另一组比较则衡量经过这两项约束的 io_uring 整体候选。

基准在每档计时后读取所有线程的 `/proc/self/task/*/status`，保存 CPU 掩码。比较脚本除了正文、最终计数、交换数和二进制/源码校验，还要求三个正文档位的资源观测齐全、所有线程 CPU 都在统一的两个 CPU 预算内、候选限额读回 `[2,2]`，以及 io-wq 线程快照不超过两类上限之和。缺少观测或超出预算会使比较失败。

## 正确性与反向对照

默认 feature 和 io_uring feature 的全目标 Clippy 均通过。原 I/O 5 项及实验 11 项测试全部通过，0 忽略；半关闭、取消计数、丢弃 ticket、背压、读取期限、小 SQ、RST 和容量边界继续保留。

16 条连接、每条 16 次请求/响应的既有测试改用 `[1,1]`，验证单个工作线程/类别仍能推进，并检查内核读回限额。只删除注册限额调用时，该测试失败，实际读回 `[8,71955]`，预期 `[1,1]`；恢复原候选后全部通过。这验证了检查能发现缺失的限制，而不是仅核对配置对象。新增非法 0 限额检查防止注册 API 悄悄保留默认值。

## 本地配对结果

同一 release 二进制 `e67d8696e9942b23dbfb88cd9cd2999af3197ba226a767e72e584ae87f0e4ac7`，CPU 0/1，1/16 条连接，三轮交错独立进程，1 秒预热、3 秒测量、20 样本。编译全部完成后才测量；两个连接数共 36 个场景通过正文、计数和新增资源检查，源码和二进制在测量前后未变。

下表为 `限制数量后的平均批次耗时 / 默认数量的平均批次耗时 - 1`，两者都显式注册相同 CPU 范围；正数表示变慢。

| 连接 | 正文 | 第 1 轮 | 第 2 轮 | 第 3 轮 | 配对中位数 |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | 1 KiB | -16.68% | -71.01% | +3.55% | -16.68% |
| 1 | 64 KiB | +9.84% | -16.68% | +63.04% | +9.84% |
| 1 | 1 MiB | +35.19% | +1.80% | -0.004% | +1.80% |
| 16 | 1 KiB | +13.31% | -96.41% | -4.24% | -4.24% |
| 16 | 64 KiB | -32.86% | -44.57% | +11.41% | -32.86% |
| 16 | 1 MiB | -6.98% | +229.67% | -21.47% | -6.98% |

该云宿主 CPU steal：单连接各进程约 8.23%～19.40%，16 连接约 12.58%～47.80%。存在 -96.41% 与 +229.67% 的大幅离群，均保留，未确认这些点各自的完整原因。因此本地数据不支持稳定的性能收益结论，也不能把全部波动仅归因于 CPU steal。16 连接的归一化往返耗时是批次均摊值，不是单请求时延或 P99。

资源边界已确认：候选所有 18 档正文快照均有 2 个 io-wq 线程；默认数量对照单连接为 2～3 个、16 连接为 5～7 个。所有观测线程的 CPU 掩码均在 0/1 内。默认数量对照实际读回限额 `[20,71955]`，不是实际创建了这些数量的线程；候选读回 `[2,2]`。

证据：[单连接](evidence/io-wq-bounded-2026-11-02/local-c1-summary.json)、[16 连接](evidence/io-wq-bounded-2026-11-02/local-c16-summary.json)、[线程/CPU/限额](evidence/io-wq-bounded-2026-11-02/local-resource-summary.json)、[CPU 快照](evidence/io-wq-bounded-2026-11-02/local-cpu-summary.json)、[全部样本、日志、反向对照及上游源码](evidence/io-wq-bounded-2026-11-02/local-raw.zip)、[逐文件校验](evidence/io-wq-bounded-2026-11-02/local-archive-manifest.json)。

## XDP 验证环境

新增独立 C 探针只加载并关闭两条指令的 XDP_PASS 程序、创建并关闭 AF_XDP socket，不附加接口、不发送数据。当前源码以 `-Wall -Wextra -Werror` 编译通过；实测普通用户和 user/network namespace 的 BPF 加载为 EPERM，隔离 Docker 中使用 BPF/NET_ADMIN/NET_RAW 能力可加载，三处 AF_XDP socket 均为 EAFNOSUPPORT(97)。关闭 FD 无错误，任务临时容器已移除。[原始结果](evidence/io-wq-bounded-2026-11-02/kernel-capabilities.json)。这说明本地不能承接真实 AF_XDP 收包测试，不是 AF_XDP 数据路径通过。

CI 新增默认关闭的 `xdp_kernel_probe=true` 输入，在 GitHub 官方 runner 的独立 network namespace 中执行同一探针，保留失败和元数据。与 io_uring 的 72 个配对场景在同一实际源码提交中验证：其中 36 个比较 readiness/限额候选，另 36 个比较相同 CPU 范围下默认/限制线程数量。

## GitHub Actions 结果

提交 `112815b891ea26e6d0e132e7a2d1487b9d6d8f9a` 的 [Actions 41529553377](https://github.com/Zelvik-Labs/edge/actions/runs/41529553377) 三个 job 全部成功：全目标 check / Clippy、单元/集成与 vendored Pingora 检查/测试、eBPF 构建、16 项实验 I/O 测试与 Clippy、72 个组件场景，以及隔离内核能力探针。16 项 I/O 测试 0 失败、0 忽略；没有关闭验证或更换基线。

计时内核 `6.8.0-1070-azure`，同一 release 二进制 `40027ecaada4ba106aacb6f3a940b7c8b86bbb1e4d27713364682be240b882d2`。四个 cohort 各三轮交错，所有用户态和 io-wq 线程的观测 CPU 掩码均在 0/1 内，24 个独立进程的 CPU steal 都为 0%。正文、计数、资源、源码和二进制一致性检查全部成功，测试提交 tracked dirty 为 false。

下表分别为 `限额候选 / readiness - 1` 与 `限额候选 / 相同 CPU 下内核默认数量 - 1` 的三轮配对耗时中位数，正数表示变慢；两列来自独立交错 cohort。

| 连接 | 正文 | 相比 readiness | 相比默认线程数量 |
| --- | --- | ---: | ---: |
| 1 | 1 KiB | +163.02% | +6.05% |
| 1 | 64 KiB | +71.54% | +4.92% |
| 1 | 1 MiB | +12.13% | -3.66% |
| 16 | 1 KiB | +64.15% | +15.61% |
| 16 | 64 KiB | +25.85% | +0.65% |
| 16 | 1 MiB | +8.53% | -1.65% |

限额候选的 36 档正文快照都是 2 个 io-wq 线程，读回 `[2,2]`。默认数量对照单连接为 2～310 个，16 连接为 11～394 个，读回 `[16,63801]`；这些都是结束快照，不是峰值。即便默认数量的线程更多，限制到 2 仍使小包耗时增加，不能将线程数量与性能作简单因果等同。1 MiB 的限额消融在三个配对中包含相反方向变化，也不据其中位数宣称稳定收益。相比 readiness 的全部 18 个配对都更慢。

原生内核能力 job 在 Ubuntu 22.04 / 6.8 上确认最小 BPF 加载和 AF_XDP socket 创建成功，关闭 FD 无错误。这仅建立后续真实 ring 测试的环境条件，不是 AF_XDP 收发或硬件零复制通过。

计时产物 `13182607633` 为 358,615 字节、664 个原始文件，SHA256 `9b8149455398b82db184818a57cdfb2b926a87f5333ee8714d2571a9be9a6a6b`；内核探针产物 `13181111913` 为 939 字节，SHA256 `f8906c8ac2756a73955a1352c77e4d9da4abff0629dad2e352f53eb93f24e5ce`。均已与 GitHub 元数据核对并保留原始 ZIP。[实际 job/step/提交](evidence/io-wq-bounded-2026-11-02/ci-validation.json)、[计时原始 ZIP](evidence/io-wq-bounded-2026-11-02/ci-raw.zip)、[资源与 CPU 范围](evidence/io-wq-bounded-2026-11-02/ci-resource-summary.json)、[CPU 快照](evidence/io-wq-bounded-2026-11-02/ci-cpu-summary.json)、[单连接/readiness](evidence/io-wq-bounded-2026-11-02/ci-comparison-c1-summary.json)、[16 连接/readiness](evidence/io-wq-bounded-2026-11-02/ci-comparison-c16-summary.json)、[单连接/限额消融](evidence/io-wq-bounded-2026-11-02/ci-workers-c1-summary.json)、[16 连接/限额消融](evidence/io-wq-bounded-2026-11-02/ci-workers-c16-summary.json)、[原生能力 ZIP](evidence/io-wq-bounded-2026-11-02/ci-kernel-raw.zip)。所有单轮数据与离群值均保留。

## 当前结论与剩余工作

已补齐实验后端的显式资源约束和观测，但 CI 没有证明性能提升，`[2,2]` 也不是本轮小包场景的更快候选。共享 io_uring SPLICE 在已测档位仍慢于 readiness，不接入默认 TCP/SNI；不再凭线程减少就推荐这一候选。该结论只涵盖本轮请求/响应组件，不能外推到所有 io_uring opcode、长流或更大连接数。后续优先验证 AF_XDP 的 Tokio readiness、批量与完整 HTTP/缓存路径。

本提交尚未验证 AF_XDP 真实 RX、硬件 NEED_WAKEUP、唤醒线程对照或硬件零复制；隔离 ring 探针已在后续独立分支推进。RKE 继续暂缓。没有批准或实施生产功能降级，也没有整体超过 Nginx/OpenResty 的结论。
