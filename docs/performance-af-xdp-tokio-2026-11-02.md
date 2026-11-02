# AF_XDP 的 Tokio 唤醒对照（2026-11-02，第 6 轮）

本轮基于 `edd7f234519fac4d329ff3b914c1e226f57d3084`，在 `perf/xdp-tokio-20261102` 分支增加独立原生基准，不改变节点的唤醒后端。第 5 轮已建立真实 XSK 的 RX/TX、fill recovery 和资源销毁功能验证，本轮测量线程/channel 与 Tokio readiness 在同一负载下的成本。

## 已确认的问题与候选范围

现有 `AfXdpWakeWorker` 为每个队列创建一个 OS 线程、一个 socket FD 副本、两个取消管道 FD 和容量 1 的 Tokio channel。与此同时，队列主循环本来就会执行 `poll_and_consume(...,0)` 与 fill `produce_and_wakeup`。因此候选保留零超时 poll 与驱动唤醒，只对照如何从空 RX 等待下一次事件，不能把去掉线程等同于去掉所有 poll。

三个方案使用同一个 release 二进制、同一份 C UMEM/ring 和 AF_PACKET 客户端：

| 方案 | 等待来源 | 就绪状态 |
| --- | --- | --- |
| worker | 节点当前的 owned poll worker/channel | 原实现 |
| readable | Tokio `AsyncFd`，只注册 READABLE | 保留 guard，观察 RX 空后清除 |
| read-write | Tokio `AsyncFd`，注册 READABLE/WRITABLE | 同样正确处理 RX guard，作为默认 RW interest 对照 |

worker 的运行时代码由构建脚本从节点源码自动提取，只有模块内文档注释变为普通注释，测试模块另行执行。比较程序会核对实际编译的生成文件与节点源码相符，并冻结原文件、生成文件和二进制摘要，防止手工维护的基线漂移。C 层复用第 5 轮 fixture，并将 socket 设为非阻塞；完整正文、序号与 TX 完成计数均保留。所有注册 FD 都持有 UMEM 的共享所有权，在重复 FD 关闭后才释放原生内存。

预期资源差异是每个队列减少一个线程、两个取消管道 FD 与 channel 通知；实际线程、FD、亲和性及耗时须看测量结果，不预设候选更快。

## 控制、负载与解释边界

功能控制先确认 8 帧全部进入 RX，读取一帧后故意提前清除 guard，在没有新 RX 和没有 TX 的条件下观察是否丢失边沿。负面对照使用纯接收，避免 TX 写方向唤醒干扰。Linux v6.8 `net/xdp/xsk.c` 的 `__xsk_generic_xmit` 在发送帧且 TX 可写时调用 `sk_write_space`，所以发送回复后再测试“没有新事件”不能隔离 RX 清除问题。

随后用另一组 8 帧验证正确保留 guard：每次只排空一帧并让出运行时，在观察到 RX 空之后清除；核对 8 次完整回复、TX CQ 与空闲状态。READABLE 与 RW 两种注册都运行同样控制。

性能比较为三轮交错独立进程，依次平衡三个方案的顺序；统一前两个允许 CPU、每帧 128 字节、单条串行请求/响应，预热 2,000 次后测量 100,000 次。客户端与 relay 共用 current-thread Tokio runtime，只有 worker 方案多一个唤醒线程。正文不经过 channel；channel 只携带唤醒。记录计时阶段等待次数并要求至少 `roundtrips-1`，验证计时请求覆盖唤醒等待路径，避免只测到同一线程先发送再立即读取的忙路径。

报告 mean/RPS 和所有 RTT 样本；P99 使用 nearest-rank 的排序索引。单请求 RTT 包含同进程客户端、内核 veth 和 relay，平均迭代耗时还包含公共计时/样本记录开销。CPU 快照覆盖整个子进程的构建后运行，包括 setup 与输出，不能当作纯服务端 CPU。资源检查在结束计时后、关闭 worker/注册前执行，保存所有线程 CPU 掩码。源码、生成的 worker 和二进制在比较前后核对。

这是 copy-mode veth 唤醒组件，不是完整 Edge、HTTP 缓存、Nginx/OpenResty 或物理 NIC 零复制容量。生产唤醒线程仍需保留到实际节点集成与匹配负载验证完成；硬件 RX NEED_WAKEUP 没有在本环境验证。这里的 Tokio 是 fd 事件层，H3 的协议库没有因本轮发生替换。

## 本地检查与 CI

独立工程位于 `scripts/perf/afxdp-tokio`，不加入节点 workspace 或生产依赖。使用同版本 Tokio 1.53.1；本地所有 Rust 构建经工作区 mbx wrapper，统一 `/workspace/edge/target`、jobs=2、测试线程=2 和 x86-64-v2，缓存继续保留。

8 项本地测试通过、0 忽略：原 worker 的 6 项生命周期/错误测试，以及默认 RW 不会自行使空 UDP socket readable、部分突发读到 WouldBlock 后才清除的 2 项 API 语义检查。独立工程 all-targets Clippy 通过，C 以警告视为错误编译，release 链接完成。最初的基准类型推断、测试模块导入和 native 链接边界问题已在本地修复；没有关闭 lint。

本地 cloud 内核的 AF_XDP 仍返回 EAFNOSUPPORT(97)，驱动能保留失败结果并清理私有 veth，比较汇总为失败、耗时为空，未把不支持写成通过或零耗时。这个环境不能提供本轮原生性能结论。mbx 对包外基线源码提示一个缓存条目缺少稳定路径映射，Cargo warm target 仍保留；没有因此清缓存或掩盖提示。

[本地检查](evidence/af-xdp-tokio-2026-11-02/validation.json)、[冻结二进制与输入](evidence/af-xdp-tokio-2026-11-02/candidate-publish-manifest.json)、[不支持时的完整运行记录](evidence/af-xdp-tokio-2026-11-02/local-unsupported-runs.json)、[原始日志/反向控制准备/上游源码](evidence/af-xdp-tokio-2026-11-02/local-raw.zip)、[逐文件校验](evidence/af-xdp-tokio-2026-11-02/local-archive-manifest.json)。本地冻结二进制 SHA256 为 `cc07c9a3dab4b67f16a461daa97045de58699ebf460f33b9b09ecb1b9012a298`，不是未来 CI 的二进制。

新增默认关闭的 `xdp_tokio_benchmark=true` 输入，在 Ubuntu 24.04 官方 runner 的隔离 netns 执行。单测、Clippy 与 release 编译全部完成后才测量，上传功能控制、逐次 RTT、资源、错误及摘要；节点既有全仓 CI 门禁继续运行。

## 原生 CI 结果

提交 `7daf4b80b1971aa45e3d12086acbf2bf48f16de5` 的 [Actions 41542718770](https://github.com/Zelvik-Labs/edge/actions/runs/41542718770) 三个 job 全部成功：原生 Tokio 对照、节点 all-targets check / Clippy、常规单元/集成与 vendored Pingora 检查/测试及 eBPF 构建。独立基准的 8 项测试 0 失败、0 忽略。原生内核 `7.0.0-1017-azure`，Tokio 1.53.1、Rust 1.99.1、libxdp 1.4.2 / libbpf 1.3.0。三个方案都使用 CPU 0/1 和同一二进制 `32840be8f9d649ab59ef12adfff3e5bc96956b20a74fd5f9bee25ddfa08c91ad`。

两个真实 XSK 功能控制均通过：纯 RX 的提前清除确实导致等待不到新边沿；正确保留 guard 的 8 次突发回复、TX 完成和最后空闲状态均通过。没有借用 TX 通知为反向控制补发事件。9 个性能进程全部成功，各自完整校验 102,000 次请求/回复（2,000 预热、100,000 计时），合计 900,000 个计时 RTT 样本。源码、生成的 worker 与二进制在前后未变，tracked dirty 为 false，9 个性能进程 CPU steal 均为 0%。

| 轮次 | worker 均值 | READABLE 均值 | RW 均值 | READABLE 耗时变化 |
| --- | ---: | ---: | ---: | ---: |
| 1 | 26.299 µs | 8.447 µs | 8.449 µs | -67.88% |
| 2 | 25.570 µs | 8.316 µs | 8.507 µs | -67.48% |
| 3 | 25.523 µs | 8.281 µs | 8.509 µs | -67.56% |

READABLE 的三轮配对耗时中位数为 **-67.56%**，对应吞吐约 **3.08 倍**；RW 的配对中位数为 -66.73%。worker 约 38.0～39.2 千次往返/秒，READABLE 约 118.4～120.8 千次/秒。READABLE 的 P99 为 9.534～9.745 µs，worker 为 33.822～35.784 µs，配对 P99 变化中位数 -71.81%。这些是同进程 copy-mode 组件成绩，不能外推为整个 Edge 或每个业务请求提升 3 倍。

每个 worker 进程结束计时后都有 2 个线程、18 个 FD；两个直接 readiness 方案均为 1 个线程、16 个 FD。绝对值包括测试客户端和运行时公共资源，差值对应一个唤醒线程与两个取消管道 FD。worker 在 100,000 个测量请求中进入等待 199,210～199,772 次，两种 readiness 都是 100,000 次；所有测量请求确实走过唤醒等待。

源码上 worker 在 RX 仍就绪时可以重复发送 hint，容量 1 的 channel 保证有界，但不会保证一次请求只有一次 hint。这与观测到约两次等待相符；省去线程/channel 这一组变化值得继续集成测试。尚未单独分配线程调度、重复通知和其他步骤各占多少微秒，不把全部收益归因于一个计数。

产物 `13187386740` 为 1,245,502 字节、58 个文件；SHA256 `87ef945fe7bb74da1435d294e22f898b6897a5dc7c274b9738978c85bb04f58d` 与 GitHub 元数据核对成功。90 万个 RTT 样本的数量、P50/P99、计时总量与帧/TX 完成总数已独立复核。P50 按排序后的上中位数取值，P99 为 nearest-rank。

[原始 ZIP](evidence/af-xdp-tokio-2026-11-02/ci-raw.zip)、[逐文件校验](evidence/af-xdp-tokio-2026-11-02/ci-archive-manifest.json)、[配对汇总](evidence/af-xdp-tokio-2026-11-02/ci-summary.json)、[完整运行索引](evidence/af-xdp-tokio-2026-11-02/ci-runs.json)、[线程/FD/等待次数](evidence/af-xdp-tokio-2026-11-02/ci-resource-summary.json)、[CPU](evidence/af-xdp-tokio-2026-11-02/ci-cpu-summary.json)、[实际 job/step 与库版本](evidence/af-xdp-tokio-2026-11-02/ci-validation.json)。

## 旧实现的代码证据与下一步

历史提交 `5fd82df1d2cfbb75fa8787cb9d26bfa9c296f950` 使用 `AsyncFd::new(...).ok()`，等待也用 `.await.ok()`，返回 guard 后立即 `clear_ready`。错误被转成 None，ready 错误时 select arm 可以立即完成；清除前也没有在该 guard 之后观察 RX 空。后续 `18636f1975a92c5eb72443b140e211e96503fb0e` 改为线程并在注释中归因于永久 EPOLLOUT。[历史差异片段](evidence/af-xdp-tokio-2026-11-02/history-source-evidence.json)。

当前匹配实验中，正确处理 guard 的 RW 与 READABLE 都稳定通过，且耗时接近，因此不支持把“RW 注册本身必然使 readable 自旋”作为一般结论。历史代码的错误吞没和过早清除属于需要避免的实现问题；本轮没有重放整个旧节点来确定当时 100% CPU 的全部原因，不能把组件反向控制等同于历史事故的完整复现。

下一阶段把已验证的状态处理接入节点实验路径，保留当前错误可观察性、队列故障后的 abort/RST、FD 生命周期、fill 驱动唤醒和原协议能力，再做实际 TCP/HTTP/UDP/H3 对照。当前默认节点仍用 owned worker，本轮结果足以推进集成，尚不能替代完整节点、硬件和长期稳定性验证。

本轮确认了组件收益，尚未把候选用于默认节点；没有批准或实施功能降级。RKE 继续暂缓。
