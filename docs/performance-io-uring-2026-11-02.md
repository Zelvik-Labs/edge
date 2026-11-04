# 共享 io_uring splice 候选（2026-11-02，第 3 轮）

本轮从 `1252e5f1cd0c38c186ad5a09f7e13c17af234428` 开始，在独立分支 `perf/io-uring-20261102` 实现候选。现有代理仍使用 readiness splice；io_uring 在 I/O 库中作为默认关闭的实验 feature，没有接入节点默认 TCP/SNI 选择。性能结论需同时看单连接和多连接批量场景。

## 实际内核能力和实现

使用 crates.io 未撤回的 `io-uring 0.7.15`，发布于 2026-09-07，checksum `ed3bd0ecfbb87805f538bb7b32e5239ca0763890c623e349860ecba69469f2bb`。根锁文件加入该包，既有依赖版本保持。另核对 `tokio-uring 0.5.0`，本轮没有把它作为运行时引入。

云端已实际执行 socket→pipe→socket 的 18 字节完整传输，随后对空闲 POLL 请求执行 ASYNC_CANCEL，核对目标 `-ECANCELED` CQE 与取消成功 CQE。注册探针确认 SPLICE、POLL_ADD、ASYNC_CANCEL、RECV 和 SEND 可用。不是仅凭 `io_uring_setup` 成功判断数据通路可用。

候选每个 worker 创建一个 ring，由多条连接共享提交和完成批次。ring 在 issuer 线程内创建，默认使用 SINGLE_ISSUER / DEFER_TASKRUN。控制通道有界，eventfd 只通知新连接、取消和停止，正文不经过用户态 channel。每条连接仍使用两根 pipe，处理部分写、双向 EOF 和目的端 `SHUT_WR`。计数只累计完成队列确认的目的写；取消时仍处理已成功写入的 CQE，再释放资源并提交 observer 的最后余量。

socket 和 pipe 使用注册文件槽位，既减少重复文件查找，也保证 io-wq 延迟查找输入文件时不会因原 FD 被复用而指向另一连接。槽位在目标 CQE 和取消 CQE 都完成后复用。读取期限用有界的 deadline 集合管理，成功读取或取消时移除，避免长期连接积累过期 timer 项。小 SQ 满时先提交而不等待，防止尚未提交的活跃 I/O 排在空闲 POLL 后面而停滞。提交、取消或能力错误均显式返回，没有失败后静默换引擎。

## 为什么 io_uring 未必胜过直接 splice

核对 Linux [v6.18.57](https://github.com/gregkh/linux/blob/v6.18.57/io_uring/splice.c) 和 [v6.8](https://github.com/torvalds/linux/blob/v6.8/io_uring/splice.c) 上游：`__io_splice_prep` 设置 `REQ_F_FORCE_ASYNC`。两者分别对应云环境和已观察到的 GitHub runner 内核系列。SPLICE 会经过 io-wq；不能把节省 `io_uring_enter` 的批量收益等同于减少所有调度成本。

```mermaid
flowchart LR
  A[readiness worker] -->|同步非阻塞 splice| K[内核 socket / pipe]
  B[共享 ring worker] -->|批量 SQE| Q[io-wq]
  Q -->|do_splice| K
  Q -->|CQE| B
```

这解释了为何本轮同时记录线程、FD、批量提交和正文计数，而不是预设换库会更快。完整节点的路由、TLS、TOA、资源准入和统计守卫尚未迁入该候选，不能拿组件成绩声明整个 Edge 已换后端或超过 Nginx/OpenResty。

## 已完成的验证

原 I/O 5 项测试与新增 10 项测试全部通过：最终响应与半关闭、取消期间写计数、丢弃 ticket 释放 socket、小 ring 下活跃连接推进、压力读取期限、停止并清空连接、512 KiB 正文经过 4 KiB pipe、同一 worker 的 16 条并发连接、容量拒绝后既有连接继续服务，以及对端 RST 保持可观察错误。默认 feature 与启用 io_uring 的全目标 Clippy 均通过，没有新增 warning 或忽略测试。

基准扩展为 1 / 16 条连接，各自采用同一 release 二进制对比 readiness 与 io_uring，统一两个 CPU 和 1 KiB / 64 KiB / 1 MiB 正文，三轮交错独立进程。16 连接共享一个 ring，测量迭代中不新增 task、Arc clone 或 Vec 分配。每个连接校验完整正文、最终双向字节和交换数；16 连接的单次迭代是一个并发批次，数据另外给出归一化往返耗时。

## 本地结果

36 个场景全部成功，测试期间源码与二进制校验和未变化。下表是每轮 `io_uring 平均批次耗时 / readiness 平均批次耗时 - 1`，正数表示变慢；中位数按三个配对变化计算：

| 连接 | 正文 | 第 1 轮 | 第 2 轮 | 第 3 轮 | 配对中位数 |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | 1 KiB | +97.96% | +28.52% | +240.47% | +97.96% |
| 1 | 64 KiB | +42.26% | +16.69% | +39.26% | +39.26% |
| 1 | 1 MiB | +32.48% | +48.36% | -57.78% | +32.48% |
| 16 | 1 KiB | +32.07% | +11.88% | +274.20% | +32.07% |
| 16 | 64 KiB | +53.91% | +55.89% | +34.18% | +53.91% |
| 16 | 1 MiB | +3.91% | +68.52% | +47.22% | +47.22% |

16 连接的 `mean_per_roundtrip_ns` 是批次耗时除以 16，表示均摊耗时，不是单请求时延分布或 P99。所有快慢轮次都保留，未用单次较快结果覆盖整体判断。

进程观测也确认了源码中的工作队列结构。第 1 轮单连接 readiness 为 18 FD、5 个线程；io_uring 为 20 FD、8 个线程，其中一个 `edge-uring` 和两个 `iou-wrk`。16 连接对应 138/140 FD；io_uring 实际有 7 个 io-wq 线程，加上专用 ring worker，比 readiness 多 8 个线程。线程数包含同一客户端/回源/统计进程的公共开销，不能当成整个 Edge 的生产线程总数。

批处理确实发生：第 1 轮 16 连接、1 KiB 时，123,472 次已校验往返对应 258,963 次提交和 987,727 条 CQE，约 3.81 CQE/提交；单连接则接近一条 CQE/提交。本地结果说明，这些批量收益还没有抵消该 SPLICE 路线的整体成本。io-wq、专用用户态 worker 与状态机的具体耗时比例尚未拆分，不能把所有回退都归因于单一因素。

[单连接汇总](evidence/io-uring-2026-11-02/c1-summary.json)、[16 连接汇总](evidence/io-uring-2026-11-02/c16-summary.json)、[线程/FD/批量归一化结果](evidence/io-uring-2026-11-02/resource-summary.json)、[CPU 快照](evidence/io-uring-2026-11-02/cpu-summary.json)、[全部原始样本/日志/探针与内核源码](evidence/io-uring-2026-11-02/local-raw.zip)、[逐文件校验清单](evidence/io-uring-2026-11-02/local-archive-manifest.json)。

## GitHub Actions 复核

提交 `b88f24b8de525f71941948e231cbfbda1ee635e1` 的 [Actions 41524209297](https://github.com/Zelvik-Labs/edge/actions/runs/41524209297) 两个 job 均成功：XDP eBPF 构建、all-targets check / Clippy、常规单元/集成与 vendored Pingora 检查/测试，以及实验 feature 的 15 项测试和 Clippy。日志接口返回的常规测试日志在末尾前截断，因此本轮不据该前缀声明全仓测试总数；15 项实验测试的完整日志在基准产物内，0 失败、0 忽略。

runner 内核 `6.8.0-1070-azure`，两个允许 CPU 为 0/1。同一 release 二进制分别测量两种引擎，各连接数三轮交错，共 36 个场景全部通过。12 个独立进程的 CPU steal 均为 0%，源码与二进制在测量前后未变，tracked dirty 为 false。这里的两个 CPU 是用户态进程的 affinity；第 3 轮没有显式注册 io-wq affinity，也没有记录内核线程 CPU 掩码。后续核对 Linux 6.8 `io-wq.c` 发现其初始化使用 `cpu_possible_mask`，因此不能确认该轮 io-wq 的有效 CPU 范围与进程一致。第 4 轮将显式注册并逐线程读取实际掩码，原始数据保持不变。

| 连接 | 正文 | 第 1 轮 | 第 2 轮 | 第 3 轮 | 配对中位数 |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | 1 KiB | +121.26% | +121.09% | +119.99% | +121.09% |
| 1 | 64 KiB | +66.16% | +67.95% | +66.78% | +66.78% |
| 1 | 1 MiB | +3.51% | +7.47% | +6.10% | +6.10% |
| 16 | 1 KiB | +34.95% | +37.11% | +41.72% | +37.11% |
| 16 | 64 KiB | +11.53% | +17.60% | +15.09% | +15.09% |
| 16 | 1 MiB | +9.42% | +9.80% | +10.94% | +9.80% |

该内核下出现了更明显的线程放大：18 个 io_uring 正文场景在观测时有 8～175 个 `iou-wrk` 线程。第 1 轮单连接 1 KiB 为 120 个，16 连接 1 KiB 为 123 个；公共线程仍为 5 个，候选另外有一个 ring worker。它们是测量结束时仍存活的线程快照，不是峰值。此候选尚未设置 `IORING_REGISTER_IOWQ_MAX_WORKERS`，所以不能把这些结果等同于所有经过调优的 io_uring 实现。下一项明确的消融是给内核工作队列设置上限，保留相同正文、取消和批次检查，再测其对资源与耗时的影响。

16 连接第 1 轮 1 KiB 的提交批量达到约 4.79 CQE/提交，仍比 readiness 慢；这再次说明减少提交次数不代表整体变快。没有测得各调度阶段的独立 CPU 成本，不给线程放大分配未经测量的因果百分比。

产物 `13180162720` 为 175,857 字节，334 个原始文件；SHA256 `88edd34cb12dc79b0a76e2e3c94cc99b355bcf9d1b74eef0e9e5b822a315a0c6` 已与 GitHub 元数据核对。证据：[原始 ZIP](evidence/io-uring-2026-11-02/ci-raw.zip)、[校验清单](evidence/io-uring-2026-11-02/ci-archive-manifest.json)、[单连接](evidence/io-uring-2026-11-02/ci-c1-summary.json)、[16 连接](evidence/io-uring-2026-11-02/ci-c16-summary.json)、[线程/FD/批量](evidence/io-uring-2026-11-02/ci-resource-summary.json)、[CPU](evidence/io-uring-2026-11-02/ci-cpu-summary.json)、[实际 job/step 与提交记录](evidence/io-uring-2026-11-02/ci-validation.json)。基准二进制 SHA256 为 `5692eced65a271c960415008b051a844fa972e830f68cd26fa25a2b1f497a4c4`。

第一次运行 [41523227311](https://github.com/Zelvik-Labs/edge/actions/runs/41523227311) 在测试准备阶段请求 GitHub `/meta` 返回 HTTP403，未保留响应正文，无法确认具体服务端原因；随后缺少产物又造成上传错误。已改为使用 Actions 自带 token 获取 SSH 主机密钥，保留失败的状态与限流头；无产物时不执行上传。重跑成功，没有关闭凭据验证或改用不可信 SSH 密钥。失败与修复不涉及运行时性能代码。

基于两处环境的完整结果，候选保留在默认关闭的实验分支，不接入默认代理。新增可选 `io_uring_benchmark=true` 输入已实际验证，样本和失败均随产物保留。本轮没有批准或实施生产功能降级；没有完整 Edge 或 Nginx/OpenResty 容量结论。

云环境另已实际验证：独立 user/network namespace 中 BPF 加载返回 EPERM；隔离 Docker 中以 BPF/NET_ADMIN/NET_RAW 能力可加载最小 XDP 程序，但两处 AF_XDP socket 均返回 EAFNOSUPPORT。未附加接口，临时容器已清理。这限定了本地 AF_XDP 验证能力，不代表真实 NIC 零复制可用。RKE 继续暂缓；真实 AF_XDP 唤醒、完整 HTTP/缓存矩阵与长期稳定性仍需后续验证。
