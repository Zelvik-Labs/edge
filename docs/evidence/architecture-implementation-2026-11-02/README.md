# 架构优化实现的测量证据

结论和限制见[完整报告](../../performance-architecture-implementation-2026-11-02.md)。
`measurement-validation.json` 汇总 96 个独立生产对照场景：91 个通过、5 个旧版 H2 场景有原生错误；新版 24 个对照场景全部通过。
另有 12 个最小引擎诊断场景和 3 个新版功能回归，分别列出，不混入容量排名。

- `h1/`：1 KiB、三轮、HIT/OFF/MISS、32 个复用连接。
- `cold-long/`：1 KiB、三轮、每批 32,768 个唯一键，H1 复用。
- `tls-h2/`：单轮 TLS H1、H2 reuse/multiplex；错误全部保留。
- `32k/`：32 KiB、单轮 H1，MISS 仅 512 个请求，属于短突发。
- `functional-128k/`、`functional-h3/`：功能回归，不能作为竞品容量成绩。
- `engine/`：同构最小 Pingora 与当时完整旧版的诊断，以及同轮两款对照；保存加入空闲观测前的矩阵驱动。
- `metadata.json`、`candidate-source.json`：生产二进制、构建参数、驱动和源码摘要；当前文件不能代替旧阶段源码。
- `production-delta.patch`：相对冻结旧版六个生产源文件，加上新编码策略和模块声明的改动；不是整个工作区相对 HEAD 的全部差异。
- `*release-source-tests.log`、`clippy-clean.log`、`python-tests.log`：最终验证；旧 UDP fixture 失败和首次构建预算中止的日志也保留。
- `gc-applied.json`：实际对象缓存回收；没有删除受管 target 或增量目录。

每组保存原始 `index.json`、原样 manifest、可移植的 `index.portable.json`，以及重算的 `results.json` / `cases.csv`。
原始索引含本机会话绝对路径；跨工作区复算使用相对于索引的可移植 manifest 路径：

```sh
python3 scripts/perf/summarize_architecture_candidates.py \
  --index docs/evidence/architecture-implementation-2026-11-02/h1/index.portable.json \
  --output /tmp/edge-architecture-h1-recomputed
```

预热前 MISS 与负载窗口 HIT 可以同时成立，CSV 明确分开 `cache_probe_before_warm` / `cache_probe_after_load`。
HIT 还须验证预热后的真实 HIT 与窗口零回源；MISS/OFF 的源站计数必须守恒。
高 steal、超过 15% 吞吐波动、短于 1 秒和客户端 CPU 较高均保留质量标记；父级总配额是 4 CPU，不是独占机器。

`raw-files.sha256` 保存 1,164 个忽略目录中的压缩逐传输记录、命令、stderr、探针响应头和运行日志摘要；原始大文件留在 `.local/architecture-implementation/`。
归档不含测试私钥、控制端配置、生产凭据、原始 perf 栈内存或二进制。
`artifacts.sha256` 校验本目录归档文件；`validation.json` 记录最终源码、二进制、复算、链接与清理检查。
未完成完整新版新连接矩阵、H3 缓存、磁盘资源隔离和长期稳定性验收，没有生产部署。
