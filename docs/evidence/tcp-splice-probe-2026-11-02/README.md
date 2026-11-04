# splice 反向回归与资源探针

该独立 Cargo 项目的源文件、manifest 和锁文件与云环境实际执行的 `.local/perf-round1/probe` 字节一致。两个目录到仓库根的层数相同，因此相对路径都引用 `crates/zelvik-edge-io` 及基线冻结模块。它不进入 Edge 运行时，也不把预期失败的旧实现放进正常 CI 单元测试。

从仓库根运行工作区 Rust 包装器：

```sh
export CARGO_TARGET_DIR=/workspace/edge/target
/workspace/docs/scripts/workspace/local-rust.sh build --locked \
  --manifest-path docs/evidence/tcp-splice-probe-2026-11-02/Cargo.toml
```

生成的 `debug/edge-splice-regression-probe` 接受两个参数：`blocking|readiness` 和 `pool|half-close|resources`。

- `pool`：阻塞线程池上限为 1，双向请求和响应必须在 2 秒内完成。
- `half-close`：客户端先发送请求并半关闭，回源读到 EOF 后才发送最终响应，必须完整返回并准确计数。
- `resources`：16 条连接分别完成精确的一字节双向交换，再记录进程自身的线程、FD 增量，最后关闭所有连接并核对计数。

进程逐个独立执行。旧引擎的非零退出码原样保留作为反向对照，不能算成功请求或吞吐；新引擎必须成功。资源场景的两个引擎都必须成功。这里的 `elapsed_ms` 仅用于诊断超时，不作为吞吐基准。
