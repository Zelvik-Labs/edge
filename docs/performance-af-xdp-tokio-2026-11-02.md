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

性能比较为三轮交错独立进程，依次平衡三个方案的顺序；统一前两个允许 CPU、每帧 128 字节、单条串行请求/响应，预热 2,000 次后测量 100,000 次。客户端与 relay 共用 current-thread Tokio runtime，只有 worker 方案多一个唤醒线程。正文不经过 channel；channel 只携带唤醒。每个测量请求都必须实际进入唤醒等待路径，记录计时阶段等待次数并要求至少 `roundtrips-1`，避免只测到同一线程先发送再立即读取的忙路径。

报告 mean/RPS 和所有 RTT 样本；P99 使用 nearest-rank 的排序索引。单请求 RTT 包含同进程客户端、内核 veth 和 relay，平均迭代耗时还包含公共计时/样本记录开销。CPU 快照覆盖整个子进程的构建后运行，包括 setup 与输出，不能当作纯服务端 CPU。资源检查在结束计时后、关闭 worker/注册前执行，保存所有线程 CPU 掩码。源码、生成的 worker 和二进制在比较前后核对。

这是 copy-mode veth 唤醒组件，不是完整 Edge、HTTP 缓存、Nginx/OpenResty 或物理 NIC 零复制容量。生产唤醒线程仍需保留到实际节点集成与匹配负载验证完成；硬件 RX NEED_WAKEUP 没有在本环境验证。这里的 Tokio 是 fd 事件层，H3 的协议库没有因本轮发生替换。

## 本地检查与 CI

独立工程位于 `scripts/perf/afxdp-tokio`，不加入节点 workspace 或生产依赖。使用同版本 Tokio 1.53.1；本地所有 Rust 构建经工作区 mbx wrapper，统一 `/workspace/edge/target`、jobs=2、测试线程=2 和 x86-64-v2，缓存继续保留。

8 项本地测试通过、0 忽略：原 worker 的 6 项生命周期/错误测试，以及默认 RW 不会自行使空 UDP socket readable、部分突发读到 WouldBlock 后才清除的 2 项 API 语义检查。独立工程 all-targets Clippy 通过，C 以警告视为错误编译，release 链接完成。最初的基准类型推断、测试模块导入和 native 链接边界问题已在本地修复；没有关闭 lint。

本地 cloud 内核的 AF_XDP 仍返回 EAFNOSUPPORT(97)，驱动能保留失败结果并清理私有 veth，比较汇总为失败、耗时为空，未把不支持写成通过或零耗时。这个环境不能提供本轮原生性能结论。mbx 对包外基线源码提示一个缓存条目缺少稳定路径映射，Cargo warm target 仍保留；没有因此清缓存或掩盖提示。

[本地检查](evidence/af-xdp-tokio-2026-11-02/validation.json)、[冻结二进制与输入](evidence/af-xdp-tokio-2026-11-02/candidate-publish-manifest.json)、[不支持时的完整运行记录](evidence/af-xdp-tokio-2026-11-02/local-unsupported-runs.json)、[原始日志/反向控制准备/上游源码](evidence/af-xdp-tokio-2026-11-02/local-raw.zip)、[逐文件校验](evidence/af-xdp-tokio-2026-11-02/local-archive-manifest.json)。本地冻结二进制 SHA256 为 `cc07c9a3dab4b67f16a461daa97045de58699ebf460f33b9b09ecb1b9012a298`，不是未来 CI 的二进制。

新增默认关闭的 `xdp_tokio_benchmark=true` 输入，在 Ubuntu 24.04 官方 runner 的隔离 netns 执行。单测、Clippy 与 release 编译全部完成后才测量，上传功能控制、逐次 RTT、资源、错误及摘要；节点既有全仓 CI 门禁继续运行。实际源码提交、run、产物与结论将在执行后记录，不能沿用第 5 轮 CI 作为本轮结果。

本轮尚无可采用的节点性能提升结论，没有批准或实施功能降级。RKE 继续暂缓。
