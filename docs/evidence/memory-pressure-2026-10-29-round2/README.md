# 内存压力第二轮验证证据

对应报告：`../../memory-pressure-validation-2026-10-29-round2.md`。

- `source-metadata.json`：候选/基线 source SHA、二进制与 fixture SHA256、编译器、编译参数、内核、THP、Python 客户端版本。
- `request-pressure-summary.json`：cache/ballast/mixed × 512/256 MiB 六轮验收的每档压力、账户门输入、L1/disk、已接受响应进度、新请求成功/拒绝、final-resort 次数与 drain 数、cgroup 终态与 OOM kill。
- `paired-timing-summary.json`：Python 客户端五轮 AB/BA 配对吞吐/P99 与三轮分配计数。
- `native-summary.json`：wrk/h2load/bench-h3-load 五轮配对、每协议 task-clock、上下文切换、futex 与 CPU 采样。
- `negative-control-owner-cap.log`、`negative-control-small-pool.log`、`negative-control-h2-stream-permit.log`、`negative-control-final-resort-gates.log`、`negative-control-h2-active-stream-idle.log`：撤回对应修复或门禁后预期失败；`ticket-small-pool-tests.log` 保存小池公平性回归。
- `full-lib-tests.log`、`h3-final-tests.log`、`clippy.log`：本机全量 lib、最后 H3 清理的定向验证与 clippy 门禁；clippy 成功但仍有三条既存 `items_after_test_module` warning。
- `final-resort-triggers.log`：四组极限压力触发时的真实 working-set/headroom/sample 与 L4/H3 drain 数。
- `fast-ramp-failure-summary.json`、`fast-ramp-oom-kernel.log`：较快 ramp 下 256 MiB ballast 曾 OOM 的失败证据；其终态采集时 cgroup 已消失，systemd Result=success 不能推翻内核明确的 kill 记录。
- `final-fair-*`：严格 workspace/4 owner cap 最终二进制的 mixed/256 压力复核、原始响应完整下载和五轮配对/ native 性能；原始 JSON 在测试机 `.../round2-final-fair/`。
- `accepted-completion-summary.json`：六组原始已接受长响应在压力恢复后的完整字节数、SHA256、完成率与 cgroup 终态，另保留一步注入的 256 MiB ballast 失败；`one-step-completion-drain.log` 为该失败的最终兜底日志。
- `0-baseline-cpu.profile.txt`、`0-candidate-cpu.profile.txt`、`direct-origin.txt`：独立 CPU 采样与直连源站样本。
- `SHA256SUMS.json`：本目录文本证据校验和。

原始 JSON、逐请求延迟、perf CSV、CPU profile、服务端日志保留于专用测试机
`zelvik-dev-43:/root/zelvik-dev/pressure-results/round2-acceptance-final4/` 与
`.../round2-benchmark/`、`.../round2-final-fair/`、`.../round2-accepted-completion/`（一步注入，含 256 MiB ballast 失败）、`.../round2-accepted-completion-ramp/`（分段补验）；本目录不收录二进制、临时 TLS 私钥、运行时 home 或 perf.data。

复现命令（Linux cgroup v2 + systemd，至少六 CPU 用于 native 隔离）：

```sh
python -I scripts/perf/run_memory_pressure.py \
  --candidate /absolute/candidate-round2-opt-node \
  --driver scripts/perf/memory_pressure.py \
  --output /absolute/results/round2-acceptance --mode acceptance --rounds 1

python -I scripts/perf/run_memory_pressure.py \
  --candidate /absolute/candidate-round2-opt-node \
  --baseline /absolute/baseline-node \
  --driver scripts/perf/memory_pressure.py \
  --output /absolute/results/round2-benchmark --mode benchmark --rounds 5 --requests 4000

python -I scripts/perf/run_memory_pressure_native.py \
  --candidate /absolute/candidate-round2-opt-node \
  --baseline /absolute/baseline-node \
  --driver scripts/perf/memory_pressure.py \
  --output /absolute/results/round2-benchmark/native \
  --tls /absolute/results/round2-benchmark/tls \
  --h3-client /absolute/bench-h3-load --rounds 5

python -I scripts/perf/run_memory_pressure.py \
  --candidate /absolute/candidate-round2-opt-node \
  --driver scripts/perf/complete_accepted.py \
  --output /absolute/results/round2-accepted-completion --mode acceptance --rounds 1

python -I scripts/perf/summarize_memory_acceptance.py \
  /absolute/results/round2-acceptance --output /absolute/request-pressure-summary.json
```

验收中 `l4Pressure`/`tcpActivePerIpLimit` 用于区分内存准入拒绝与 L4 每 IP 活跃连接上限；
恢复检查点等到 L4 的 5 秒降级 dwell 结束再创建新连接。成功请求的 P99 不含被拒绝或超时请求，
必须同时读取 success/attempted/errors 字段。
