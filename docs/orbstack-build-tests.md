# 本机 OrbStack 编译与测试

此流程只验证开发源码，不打发布包、不部署，不触碰 `orb-deploy` 或授权站。
产品版本保持 `26.10.2`。

## 版本与构建边界

- Edge stable 工具链：Rust `1.99.0`、edition 2024，包含 rustfmt、clippy、LLVM 工具。
- Pingora：官方 `0.9.0` 基线加必要的既有扩展，见
  [`pingora-main/DIVERGENCE.md`](../pingora-main/DIVERGENCE.md)。没有旧 API 别名。
- XDP eBPF：仍使用 `crates/zelvik-edge-xdp-ebpf/rust-toolchain.toml`
  指定的 dated nightly 和 bpf-linker `0.11.1`；不替换成 stable。
- Linux 测试镜像：官方 `rust:1.99.0-bookworm`，mbx `1.14.0`
  的官方 Linux 归档按固定 SHA-256 校验，不使用远程编译缓存。

## 执行

从 Edge 仓库目录运行（默认 ARM64，全套验证）：

```bash
bash scripts/test-orbstack.sh
bash scripts/test-orbstack.sh amd64 all
```

也可以单独运行 `check`、`test`、`clippy`、`pingora` 阶段：

```bash
bash scripts/test-orbstack.sh arm64 check
bash scripts/test-orbstack.sh amd64 check
```

所有阶段先构建真实 eBPF 对象，并通过 `ZELVIK_EDGE_XDP_EBPF_SOURCE`
和 `ZELVIK_EDGE_XDP_EBPF_SHA256` 验证嵌入内容，不能用空对象通过构建。
ARM64 使用 generic CPU；AMD64 使用生产底线 `x86-64-v2`。
AMD64 在 Apple Silicon 上通过 OrbStack 仿真运行，耗时不能用于推断
生产机器性能。

默认容器限制为 2 CPU、4 GiB 内存；可通过
`ZELVIK_EDGE_BUILD_CPUS`、`ZELVIK_EDGE_BUILD_MEMORY` 调整。不要让多个
大规模编译任务争用 OrbStack 中已有服务的内存。

镜像构建上下文为空，源码只作为只读 bind mount 提供。容器将源码
复制到独立测试工作区卷；宿主的 target 符号链接、配置、运行目录、
本地数据和 Git 元数据不会复制进去。测试不开放宿主监听端口。

私有 `zelvik-suite` 依赖先在宿主预检：宿主 Git 必须能读取
`https://github.com/Zelvik-Labs/protocol`（例如先运行 `gh auth setup-git`）。
脚本从 `Cargo.lock` 读取完整 commit SHA，抓取到临时 bare 仓库，
并为锁定的 tag 建立指向该 commit 的本地引用；上游 tag 移动不会改变依赖。
抓取失败立即停止，尚未构建 Docker 镜像。诊断见 `protocol-fetch.log`。
`pingora` 阶段只构建独立的 `pingora-main` workspace，不解析私有依赖，
因此跳过预检、不挂载该仓库。

容器只读挂载这个临时仓库，通过仅作用于本进程及子进程的 Git URL
重写和 Cargo CLI Git 后端读取对象；不传 token、不挂载宿主 Git 配置或
Cargo Git 缓存，不改 `Cargo.toml` / `Cargo.lock` 的依赖身份。
容器在安装 nightly、构建 xtask/eBPF 前执行 `cargo metadata --locked`；
失败时所有编译阶段停止，诊断见 `dependencies.log`，成功的完整依赖图见
`dependency-metadata.json`。公共 registry 依赖仍需网络或容器 registry 缓存。
退出（包括失败或 INT/TERM）时删除临时仓库、本次容器、镜像与缓存卷；
不按容器名误删其他运行。若容器删除本身失败，保留其只读 bind 与资源
并返回非零，给出容器 ID，由人工处理后重跑，避免删掉仍在使用的 bind。

## 缓存与动态清理

每次运行的 `zelvik-edge-test-{workspace,registry,rustup,tools,mbx}-<arch>-<run>`
卷和 `zelvik-edge-orb-test:1.99.0-<arch>-<run>` 镜像都以运行 ID 命名，
结束（含失败与信号）时删除；宿主不留持久 Cargo/mbx 缓存，代价是每次
重新下载 registry 与工具链。macOS 的 Cargo/mbx 目录不挂载到 Linux；
ARM64 与 AMD64 不共享原生链接对象，运行之间也不复用。

容器默认预算：mbx action store 3 GiB、managed targets 5 GiB、incremental
1 GiB、合计 8 GiB。预算是逻辑字节的清理目标，不是实时磁盘配额；
编译过程中可能短时超出。为避免构建期间 target 被收走，自动 GC
关闭，在每个构建/测试命令退出后清理本次 workspace 并运行 GC。
失败退出也运行清理，不删除共享编译对象以外的服务数据。

原生 macOS 编译也应在命令范围内关闭自动 GC，退出后再清理。例如：

```bash
MBX_GC_AUTO=false mbx check --locked --all-targets
bash scripts/clean-build-artifacts.sh
```

宿主清理与脚本自测命令：

```bash
bash scripts/clean-build-artifacts.sh --dry-run
bash scripts/clean-build-artifacts.sh
python3 -I scripts/test-clean-build-artifacts.py
python3 -I scripts/test-orbstack-orchestration.py
```

清理脚本检查 Cargo 的 `.cargo-lock`，拒绝清理正在构建的 target；
mbx 管理的符号链接通过 `mbx clean <workspace>` 处理，普通 target
只允许在 workspace 内且名为 target。危险路径和非 mbx 外部符号链接
会被拒绝。脚本使用 Python 隔离模式，清理失败返回非零。

**不要并行启动清理和新的 Cargo 命令。** Cargo 锁检查无法阻止另一
终端在检查之后新开构建；编排脚本通过唯一容器名和串行命令保证自身
流程不发生这种竞争。原生 macOS 构建也必须等进程退出再清理。

宿主默认不运行全局 mbx GC，避免驱逐其他项目缓存。仅对专用 store
设置 `ZELVIK_EDGE_MBX_GC=1` 才让清理脚本执行预算 GC。

## 输出与验证范围

日志默认保存在工作区外的 `../artifacts/edge-orb-tests/<时间>-<架构>/`，
可通过 `ZELVIK_EDGE_TEST_RESULTS` 指定其他目录。包含各阶段日志、
清理日志、mbx 诊断/统计、GC JSON、eBPF 哈希和最终退出状态。
`check` 阶段清理 target 后再运行一次，便于观察共享编译对象的实际复用。

`all` 阶段包括：Edge 全目标 check、workspace 单元与二进制测试、
集成测试、correctness/suspicious Clippy 门禁，以及启用 rustls 的
关键 Pingora crate 检查与库测试。独立验证阶段发生失败时继续其余阶段，
每个阶段单独记录退出状态，最终仍返回非零；eBPF 等构建前置步骤或清理
失败则立即停止。脚本的离线自测使用本地 Git/Cargo 小型 fixture 验证锁定
revision 解析，并用 Docker stub 验证失败与信号清理；不执行编译、不连接
GitHub、不创建容器、镜像或缓存卷，不能替代真实 Docker/Linux 验证。

以下不能从编译或默认测试结果推断：

- macOS 检查不能替代 Linux XDP/AF_XDP 代码验证。
- eBPF 编译/嵌入成功不等于内核 verifier、真实 NIC 或零拷贝行为验证。
- 需要 API 端点/凭证或显式环境开关的测试默认不会执行真实外部请求；
  报告应单独列出，不把提前返回算作真实 API 验证。
- 无适用场景的 Rust 新稳定 unsafe/C-variadic/naked/汇编/raw-layout API
  不引入项目。当前升级采用有所有权的 UTF-8 lossy 转换及
  `Atomic::try_update`/`update`，保留原子操作的成功/失败 ordering 和
  饱和计数语义；借用输入不为使用新 API 而额外复制。
