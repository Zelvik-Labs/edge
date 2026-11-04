# H3 重试缓冲修复证据

结论与范围见[修复报告](../../performance-h3-retry-buffer-2026-11-02.md)。发行配置容量对照仍在[接收消融](../h3-recv-ablation-2026-11-02/README.md)及[前轮联合候选](../h3-stage2-2026-11-02/README.md)，本组只做正确性和警告触发诊断。

before / after 的 metadata.json 冻结各 177 项根仓源码及各自的 dev 可执行文件摘要，profile 一致。源码差异只有 h3_downstream.rs / proxy.rs；implementation.patch 为两文件完整变更，regression-tests.patch 是旧代码上先增加的测试。原始二进制和源码快照留在忽略目录，归档记录其摘要与补丁。

red-result.json 与 red-tests.log 保留旧代码 1 通过 / 2 预期失败、Cargo 101 的实际结果。after/test-results.json 记录修复后的相关测试及唯一名称；校验器确认旧 408 项都仍被覆盖。after/checks.json 记录每项检查和构建的真实返回码。

warning-comparison.json 单独记录同配置 H3 OFF 探针的特定警告数和成功请求数，不是吞吐提升估计。functional-results.json 保留基线 1 项、修复后 H3 5 项与 H1/H2 6 项，共 12 项功能场景；H3 新连接 HIT/OFF 各 2048 请求。原生错误、回源数与连接数均来自真实 manifest。

functional-index.portable.json 指向归档 manifest，原始逐响应 TSV、headers、stderr 和运行日志位于 `.local/h3-retry-buffer`，140 项摘要在 raw-files.sha256。harness-sha256.json 与 tool-sha256.json 记录使用的工具，drivers 保留源码。运行时 PEM、overrides、二进制及 perf.data 不归档。

在本目录执行 `sha256sum -c artifacts.sha256` 检查归档。工作区有完整原始数据时，可在仓库根运行 `.local/h3-retry-buffer/validate.py` 检查源码、二进制、预期失败、原测试集合、功能 manifest 和全部原始文件摘要。

没有修改 vendored Pingora、日志级别、协议库、正文校验或超时参数。没有重测并替换接收消融中的 12 次超时，也未完成本次修复的 release 容量或长期稳定性验收。
