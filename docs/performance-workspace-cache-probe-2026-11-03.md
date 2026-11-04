# 第 13 轮：缓存命中与真实编译复用

两次相同提交的独立 CI 均通过，但**第二次缓存命中后，库和程序仍被 Cargo 判为非 fresh 并重编译**。因此仅启用工作区 crate 缓存，尚未证明能解决跨检出的编译耗时；本轮不调整主验证缓存策略，也没有运行时性能优化结论。

源码 `d73308e04d8ffeeece0eb2dbc422e32a9d4fd28e`，独立分支 `perf/cache-workspace-20261103`。首轮 [41583824344](https://github.com/Zelvik-Labs/edge/actions/runs/41583824344) 与同提交复测 [41584455927](https://github.com/Zelvik-Labs/edge/actions/runs/41584455927) 均成功。新增 workflow 的 push 只限该分支与该文件，亦可手动触发；未改动 Edge 运行时或现有 CI。

## 条件与控制

当前 `Swatinem/rust-cache@v2` 的 `cache-workspace-crates` 默认 false，现有 CI 未显式覆盖。[上游输入](https://github.com/Swatinem/rust-cache/blob/v2/action.yml)与[保存逻辑](https://github.com/Swatinem/rust-cache/blob/v2/src/save.ts)已只读核对。这里仅为一个无依赖、独立 workspace 的小库与程序启用该选项，不推断 Edge 全量缓存的效果。

Rust 1.99.1、Ubuntu 22.04、x86-64-v2、jobs=2。每次生成相同库 v1 与调用它的程序，再恢复缓存。依次执行四次真实构建：初始 v1、改为 v2、恢复 v1、完全不改源码的同任务再构建。每步保留 Cargo JSON、日志、源文件/二进制摘要和 profile；运行程序断言实际返回预期版本。最后一个 v1 保留到缓存保存，首任务全部结束后才启动同提交复测。

没有规范化源码时间戳，没有绕过 Cargo 输入检查，没有把缓存命中当作编译成功或编译复用。该探针没有本地 Rust 编译，遵守本地低于 6GiB 不启动的保护。

| 观测 | 首次新 runner | 第二次新 runner |
| --- | --- | --- |
| action cache hit | false | true |
| 恢复的指纹文件数 | 0 | 8 |
| 初始库 / 程序 fresh | false / false | false / false |
| 改 v2 后 fresh | false / false | false / false |
| 恢复 v1 后 fresh | false / false | false / false |
| 同任务不改源码 fresh | true / true | true / true |

第二次新源码 mtime 晚于恢复的指纹文件，是继续调查的线索；当前观察未完整排除产物清理、输入指纹及其他因素，**不将时间戳写成已确认的唯一根因**。程序版本由 workflow 在运行时断言，stdout 未另行保存，独立复核不能从摘要重建其输出。

四步中的源码改变与不改变控制均正确。不同 runner 的小构建耗时不能当作可靠加速比例，也不代表完整 Edge 的构建。后续需要实际编译输入与内容缓存的验证，不能单靠扩大预算或一个开关宣称消除重编译。

## 证据、未解决与功能降级

两次 Cargo 原始消息、阶段汇总和源码摘要已独立重算。

- [独立验证](evidence/workspace-cache-probe-2026-11-03/validation.json)、[首轮条件](evidence/workspace-cache-probe-2026-11-03/first-conditions.json)、[复测条件](evidence/workspace-cache-probe-2026-11-03/second-conditions.json)、[首轮阶段](evidence/workspace-cache-probe-2026-11-03/first-summary.json)、[复测阶段](evidence/workspace-cache-probe-2026-11-03/second-summary.json)。
- [首轮原始产物](evidence/workspace-cache-probe-2026-11-03/first-raw.zip)、[清单](evidence/workspace-cache-probe-2026-11-03/first-manifest.json)、[job/step](evidence/workspace-cache-probe-2026-11-03/first-jobs.json)、[元数据](evidence/workspace-cache-probe-2026-11-03/first-artifact.json)。产物 `13207655199`：5,017 字节，SHA256 `2128e140bb20466d3cb1a986b7f9cf91ba5bf47b8ce62b7f38b2e4d223ea7eaf`。
- [复测原始产物](evidence/workspace-cache-probe-2026-11-03/second-raw.zip)、[清单](evidence/workspace-cache-probe-2026-11-03/second-manifest.json)、[job/step](evidence/workspace-cache-probe-2026-11-03/second-jobs.json)、[元数据](evidence/workspace-cache-probe-2026-11-03/second-artifact.json)。产物 `13207146674`：5,208 字节，SHA256 `a6956c5df8e8d73d3794dd90da5f113805f5373858a651bd8a10ab9652a5bfb3`。两 ZIP 与 GitHub digest 一致。

跨检出的复用问题和完整 Edge 的构建耗时尚未解决，没有采用主缓存策略改动。云端本地缓存仍按物理容量放宽、自动 GC 关闭，保留 6/5GiB 空间线；增大缓存不能增加物理磁盘容量或跳过必要编译。没有功能降级、发布、部署或 main 合并，RKE 暂缓。
