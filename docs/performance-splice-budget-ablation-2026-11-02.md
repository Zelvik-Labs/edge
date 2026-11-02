# 大包系统调用与协作预算消融（2026-11-02，第 2 轮）

第 1 轮 CI 已确认小包收益，但 1 MiB 配对耗时中位数为 +1.43%，没有明确改善。本轮继续诊断，并实际测量一个候选。结论是：**保留第 1 轮运行时实现，删除额外协作预算检查的候选不采用。**

## 系统调用追踪

对第 1 轮同一冻结二进制，分别选择旧阻塞引擎和新事件驱动引擎，以相同两个 CPU、连接复用、1 MiB 正文运行。使用 `strace -f -qq -e trace=splice,poll,epoll_wait,epoll_pwait,futex`。两者均通过完整正文和最终计数校验，各完成 885 次往返。正数 splice 返回值总和均为 3,711,959,040 字节；每个请求/响应分别经过 socket→pipe→socket，因此以 `总和 ÷ 4 ÷ 1 MiB` 归一化往返次数，包含 warmup。

| 每次往返的完成 syscall | 旧阻塞引擎 | 新事件驱动引擎 |
| --- | ---: | ---: |
| splice | 21.35 | 19.72 |
| poll | 2.56 | 0.001 |
| epoll_wait | 11.58 | 5.42 |
| futex | 4.34 | 2.47 |

新引擎未出现调用次数膨胀，正数 splice 的 256 KiB 完整批次也更多。但追踪会改变调度和分段，且进程包含客户端、echo 回源与 relay；这些数不能当成生产 syscall 成本或未追踪的吞吐排名，也不能证明第 1 轮大包波动的根因。

## 候选与消融

锁定的 Tokio 1.53.1 `runtime/io/registration.rs::async_io` 已调用 `coop::poll_proceed`，成功 I/O 后调用 `made_progress`。候选只删除 relay 每批次后的显式 `tokio::task::consume_budget().await`，保留 Tokio 自带协作机制。源码片段、完整文件校验和及前后源码快照已归档；未修改 Tokio 或其他 vendored 源码。

候选通过 5 项 I/O 行为测试和 I/O 全目标 Clippy，并用同一 release 配置重新构建。基线为提交 `2df8b242b1b0d24f199e22592b6924e592f960a2` 的冻结二进制；两者都选择 readiness 引擎。两个二进制分别独立启动，三个轮次交错顺序，同样两个 CPU、正文、连接复用、20 个样本、1 秒 warmup、3 秒测量。18 个场景全部成功，测量期间源码与二进制校验和未变。

下表为删除显式检查后相对保留检查的平均往返耗时变化，正数是回退：

| 正文 | 第 1 轮 | 第 2 轮 | 第 3 轮 | 配对变化中位数 |
| --- | ---: | ---: | ---: | ---: |
| 1 KiB | +14.93% | -58.08% | +12.55% | **+12.55%** |
| 64 KiB | +50.09% | +50.72% | +34.40% | **+50.09%** |
| 1 MiB | +2.44% | -10.94% | +5.01% | **+2.44%** |

## 同源重建与第二组对照

复核发现，相同基线源码恢复后重新构建，二进制 SHA-256 从 `909ab226dcdd649c82cfda9f7010c5c6cb0c01cdd621760535ea0834ba71409d` 变为 `42ff372952ce2280073248949004dc63ebb13fbf8dbc4a809d52258998c75a73`，`.text`、`.rodata` 和 `.eh_frame` 的校验和也不同。Cargo release 配置相同，冷/热构建日志中的 mbx incremental 状态不同；具体生成差异尚未定位，不能直接归因给某个编译选项或缓存机制。

因此又使用重建后的基线，与原候选冻结二进制追加三轮独立交错对照。两个构建日志都记录了 warm/incremental 状态，配置和输入校验和已归档，但这仍不证明代码生成差异完全排除。18 个追加场景全部成功，三个配对变化中位数如下：

| 正文 | 第 1 轮 | 第 2 轮 | 第 3 轮 | 配对变化中位数 |
| --- | ---: | ---: | ---: | ---: |
| 1 KiB | +50.16% | -77.21% | -50.88% | -50.88% |
| 64 KiB | -2.07% | -75.43% | +29.67% | -2.07% |
| 1 MiB | +15.10% | +66.46% | -4.46% | **+15.10%** |

两组共 36 个场景均成功，但云端波动、调度与生成产物影响尚未分离；不能把第一组 64 KiB 的回退直接认定为删除协作检查的因果效果。大包在两组中的中位数都未受益，整体也未见稳定收益，因此不采用候选。库源文件已与第 1 轮提交逐字节一致；没有删除额外检查。

该候选仅做本地云端消融，未进入 GitHub Actions；没有把第 1 轮 CI 结果移用到被拒绝的候选。保留的运行时仍对应已通过的 [Actions 41508571673](https://github.com/Zelvik-Labs/edge/actions/runs/41508571673)。恢复相同源码后，为核对生成产物重建了组件基线，没有重复功能测试；后续提交仅补充报告与证据。

## 证据和剩余工作

[syscall 归一化结果](evidence/splice-budget-ablation-2026-11-02/syscall-summary.json)、[首组消融](evidence/splice-budget-ablation-2026-11-02/budget-summary.json)、[追加对照](evidence/splice-budget-ablation-2026-11-02/budget-warm-summary.json)、[同源重建核对](evidence/splice-budget-ablation-2026-11-02/baseline-rebuild.json)、[ELF 段校验和](evidence/splice-budget-ablation-2026-11-02/section-hashes.json)、[首组均值和 CPU 快照](evidence/splice-budget-ablation-2026-11-02/budget-runs.json)、[首组二进制及源码校验和](evidence/splice-budget-ablation-2026-11-02/budget-metadata.json)、[原始日志/样本/执行脚本/源码快照 ZIP](evidence/splice-budget-ablation-2026-11-02/raw-data.zip)、[逐文件校验清单](evidence/splice-budget-ablation-2026-11-02/archive-manifest.json)。两组均值、CPU 快照与 metadata 都在 ZIP 中，以实际 `.local/perf-round2` 为根；复现脚本需解压到同一目录层次，基线冻结二进制另见第 1 轮记录。

大包吞吐、同源构建产物变化的来源、io_uring 数据通路、真实 AF_XDP NIC、H3 后端选择和长期稳定性仍需继续推进。已确认本云环境可建立 io_uring ring，但尚未接入数据通路；没有新增功能降级。
