# 内存压力验证证据

对应报告：`../../memory-pressure-validation-2026-10-29.md`。

- `source-metadata.json`：source SHA、相同依赖锁、fixture 与二进制 SHA256、工具链、编译参数、THP。
- `request-pressure-summary.json`：256/512 MiB 各三轮实际压力、RSS/PSS、PSI、events、新请求 P99、既有响应存活，以及一次单连接慢 stream 场景。
- `continuation-summary.json`：不再创建新请求的六轮压力，含每轮终态内核计数。
- `paired-timing-summary.json`：Python 客户端五轮配对吞吐/P99 与三轮分配计数。
- `native-summary.json`：native wrk/h2load/bench-h3-load 五轮配对、per-protocol CPU/上下文/futex 中位数。
- `direct-origin.txt`：直接访问 Python 测试源站的 wrk 上限样本。
- `shed-log-excerpt.txt`：真实压力下 0ms/30000ms 年龄档的 L4+H3 drain 日志。
- `cache-tests.log`、`full-lib-tests.log`、`lookup-negative-control.log`、`known-gap-reproductions.log`、`reserve-concurrency.log`、`clippy.log`：本轮回归与负对照记录。

原始 JSON、每请求延迟、perf CSV、CPU profile 与服务端日志保留于专用测试机
`zelvik-dev-43:/root/zelvik-dev/pressure-results/`。本目录不收录二进制、临时 TLS 私钥、
运行时 home 或 perf.data。测试机上的 `comparison-v3` 是源站 framing 修正前的无效样本，
未纳入任何结论；有效配对数据在 `comparison-v4`。

复现需要专用 Linux cgroup v2 + systemd 主机（至少六个 CPU 用于 native CPU 隔离）。
用同一 Cargo.lock/fixture/编译参数分别构建基线与候选；编译参数见 metadata，
Rust 本机验证仍走工作区 `scripts/local-rust.sh`。

```sh
python -I scripts/perf/run_memory_pressure.py \
  --candidate /absolute/candidate-node --driver scripts/perf/memory_pressure.py \
  --output /absolute/results/pressure --mode pressure --rounds 3
python -I scripts/perf/run_memory_pressure.py \
  --candidate /absolute/candidate-node --driver scripts/perf/memory_pressure.py \
  --output /absolute/results/continuation --mode continuation --rounds 3
python -I scripts/perf/run_memory_pressure.py \
  --candidate /absolute/candidate-node --baseline /absolute/baseline-node \
  --driver scripts/perf/memory_pressure.py --output /absolute/results/comparison \
  --mode benchmark --rounds 5
python -I scripts/perf/run_memory_pressure_native.py \
  --candidate /absolute/candidate-node --baseline /absolute/baseline-node \
  --driver scripts/perf/memory_pressure.py --output /absolute/results/comparison/native \
  --tls /absolute/results/comparison/tls --h3-client /absolute/bench-h3-load
python -I scripts/perf/summarize_memory_pressure.py /absolute/results \
  --output /absolute/summary.json
```

Python 环境需 `h2` 与 `aioquic`；native runner 需 wrk、h2load、perf。
所有测试绑定 loopback，ballast 故意触碰实际内存页，仅用于隔离测试 cgroup。
结果中成功请求的 P99 不包含被拒绝或超时请求，必须同时读取 success/errors/unattempted。
第一批请求压力包含宿主 PSI 余波与 H1 close 处理限制，详细说明见报告，不用来标定精确阈值。
