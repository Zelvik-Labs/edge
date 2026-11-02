# TCP/SNI splice：事件驱动与关闭语义（2026-11-02，第 1 轮）

本轮基线为 `6f942440df52b7797fbb46cec86cef916681deef`，对应已通过的 GitHub Actions [41497645026](https://github.com/Zelvik-Labs/edge/actions/runs/41497645026)。下文候选是基线之后的改动；候选的 CI 与性能结果分别记录，不能沿用基线的绿色状态。

## 问题与实现

原 Linux 内核 splice 每条连接使用两个 `spawn_blocking` 任务。两根 pipe 占用四个 FD，方向与关闭控制再复制六个 socket FD。空闲连接长期占据阻塞线程；某一方向读到 EOF 后，父任务会停止另一方向并对两个 socket 执行 `SHUT_RDWR`。这会阻断需要在客户端半关闭之后继续返回响应的协议。父 future 被取消时，独立阻塞任务还可能继续持有复制的 socket。

新 `crates/zelvik-edge-io` 使用 Tokio `TcpStream::async_io` 驱动非阻塞 `splice`，两个方向在同一父任务内推进。两根 pipe 由 `OwnedFd` 管理，借用已有 socket，不创建阻塞任务、不复制 socket。读到 EOF 后仅向目的 socket 发 `SHUT_WR`，继续等待反向完成；错误、显式取消或 task abort 会释放方向 future 和 pipe。

Edge 适配层保留内存、FD、共享账户和连接压力准入、压力超时策略、关闭原因和错误分类。新 FD 预算为四个额外 FD，移除没有实际阻塞任务后过时的 `CPU × 128 ÷ 2` 上限。SNI 的压力空闲超时仍然关闭；已关闭的取消通道仍不当作 L4 drain。传输计数只累计实际写入目的 socket 的字节，仍按 1 MiB 合并更新指标，observer 析构时提交不足一个批次的余量。pipe 扩容被内核拒绝时记录实际容量和 errno，使用仍然有效的内核容量；准备 pipe 失败仅在尚未消费输入时走既有备用路径。

## 验证与测量边界

新增公共 I/O 测试覆盖：只有一个阻塞线程时的双向推进、客户端半关闭后的最终响应、task abort 的 socket/observer 生命周期、可观察的读取超时，以及小 pipe 上双向大正文的精确传输。根集成测试引用同一组测试，现有 CI 会执行；另加 Edge 适配层的半关闭、取消与实际字节计数测试。

`benches/splice.rs` 在一个进程内运行客户端、echo 回源与 relay，对 1 KiB、64 KiB、1 MiB 连接复用交换比较新旧引擎。旧引擎来自上述基线的 `zero_copy` 模块，代码只经 rustfmt 排版，去除空白后与保存的原文一致。组件上下文关闭指标和压力超时；它不测 SNI 解析、TLS、路由、真实网络容量或完整 Edge 性能。

本地全目标检查通过；新 I/O 模块 5 项、TCP/SNI 19 项测试通过。移除旧阻塞任务上限之后，资源治理器 53 项测试通过，新增检查确认同等内存和 FD 条件下不再受 CPU 数引出的旧上限限制，而且收紧内存或 FD 仍会收紧准入。根单元测试仅对 `zelvik-edge` 测试包使用 opt-level=0 以缩短正确性反馈，依赖仍为 opt-level=2；它不用于 release 基准。

反向对照用同一冻结基线，每个场景连续三轮、分别启动独立进程。单阻塞线程双向交换的旧引擎三次均在 2 秒超时，新引擎三次均精确完成 7/8 字节；客户端半关闭场景的旧引擎三次均返回 `UnexpectedEof`，新引擎三次均精确完成 7/14 字节。这 6 次旧引擎失败原样保存，不能计入成功请求或吞吐。

16 条连接各自完成双向正文校验后的资源探针也运行三轮，两个引擎均成功，三轮读数完全一致：

| 进程自身资源增量 | 旧阻塞引擎 | 新事件驱动引擎 |
| --- | ---: | ---: |
| 线程 | 32 | 0 |
| FD（含两端测试 peer 与 relay socket） | 224 | 128 |
| 每条 relay 的额外 FD（扣除四个 TCP socket） | 10 | 4 |

来源：[18 次独立探针的完整结果](evidence/tcp-splice-2026-11-02/probe-results.json)、[可复现探针](evidence/tcp-splice-probe-2026-11-02/README.md)。其中新引擎 9 次成功，旧引擎 3 次资源测试成功、6 次回归场景失败。计时字段仅诊断超时，不作为性能结果。

最终版本的 Root / I/O Clippy 均通过，新 I/O 模块无新增警告；所选功能测试总计 77 项通过（5 + 19 + 53）。开发期间两处测试/探针上下文的编译错误及一处 Clippy 提示的修复前日志保留，未当作容量失败或运行时问题。

## 组件基准结果

使用同一 release 二进制（opt-level=3、fat LTO、单 codegen unit、x86-64-v2），新旧引擎分别在独立进程中执行，统一两个 CPU、连接复用和完整正文校验。三个轮次按旧/新、新/旧、旧/新交错，共 18 个场景全部成功，二进制和所列源码在测试期间校验和未变化。

下表为每轮“新引擎平均往返耗时 ÷ 旧引擎平均往返耗时 − 1”；负数表示耗时下降。末列取三个配对百分比的中位数。

| 单次正文 | 第 1 轮 | 第 2 轮 | 第 3 轮 | 配对变化中位数 |
| --- | ---: | ---: | ---: | ---: |
| 1 KiB | -2.63% | -53.51% | -36.41% | -36.41% |
| 64 KiB | -33.50% | -21.96% | -35.71% | -33.50% |
| 1 MiB | -11.00% | -24.82% | **+23.80%** | -11.00% |

1 MiB 第三轮确实回退，未剔除或重跑覆盖。当前共享云环境所选 CPU 的整组 steal 为 25.93%～29.38%，六组 cgroup 节流增量均为零。该观察不能证明回退由宿主争用造成，也不能排除引擎差异；需要 GitHub Actions 的同源复测与进一步剖析。线程/FD 的下降和两项旧 bug 的修复已被直接验证，小包耗时有收益，大包收益尚不稳定。这些数字不是完整 Edge/SNI 吞吐，也不能声明整体超过 Nginx/OpenResty。

来源：[配对汇总](evidence/tcp-splice-2026-11-02/comparison/summary.json)、[所有均值/置信区间与 CPU 快照](evidence/tcp-splice-2026-11-02/comparison/runs.json)、[CPU 摘要](evidence/tcp-splice-2026-11-02/cpu-summary.json)、[源码/二进制与参数](evidence/tcp-splice-2026-11-02/comparison/metadata.json)。原始 Criterion sample、outlier、估计文件和 stdout/stderr 均保留在对应轮次目录。

候选 GitHub Actions 的实际提交、run 和 job 结果将在执行后记录，不能用基线绿色状态替代。

独立进程比较入口为 `scripts/perf/compare-splice.py --binary <已构建的 splice bench> --output <新目录>`。默认三个轮次交错顺序；两个引擎各自独立启动，不创建另一个引擎的连接，统一绑定到相同两个允许的 CPU。保存所有 Criterion 样本、返回码、置信区间、CPU/节流快照、二进制与源码校验和。GitHub Actions `ci.yml` 的 `splice_benchmark=true` 手动输入会在编译和功能测试完成后运行，并上传原始结果；普通 CI 不额外执行基准。

## 环境与后续

云构建缓存使用 `cloud` 档位，各类别和总预算随文件系统容量扩展，mbx 的对象、target 与 learned 缓存上限为 32 GiB，自动 GC 关闭。启动前保留 6 GiB、运行中保留 5 GiB 可用空间；不在每轮结束清缓存。该策略的 26 项测试通过，不改变服务压测的 CPU、内存或业务缓存配额。

当前云环境的 `io_uring_setup(8)` 实际调用成功：`features=262143`，SQ=8，CQ=16。该结果仅证明可以建立 ring，不证明 io_uring 更快；后续需在相同正文校验、连接数、取消与背压语义下比较。真实 AF_XDP NIC 零复制容量、长期稳定性与 H3 接收消融超时根因仍未完成；本轮没有批准或实施功能降级。
