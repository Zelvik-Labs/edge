# AF_XDP 唤醒与 ring 行为验证（2026-11-02，第 5 轮）

本轮从第 4 轮实现 `112815b891ea26e6d0e132e7a2d1487b9d6d8f9a` 建立独立分支 `perf/xdp-wake-20261102`，准备真实 socket 的功能证据，不修改节点数据路径。第 4 轮 [Actions 41529553377](https://github.com/Zelvik-Labs/edge/actions/runs/41529553377) 已确认 GitHub 官方 runner 可以在隔离 netns 中加载 XDP_PASS 并创建 AF_XDP socket；该轮 io_uring 配对测量仍独立运行。

## 验证内容

`scripts/perf/afxdp-wake.c` 使用 libxdp 的 UMEM/RX/TX/FILL/CQ，强制 `XDP_COPY | XDP_USE_NEED_WAKEUP`，并显式绑定 veth queue 0。自有最小 BPF 程序仅将测试 EtherType `0x88b5` 的帧转入自有 XSKMAP，其他帧 PASS。所有帧核对固定正文和序号，回收对应 descriptor；不加载外部 `.o`、不依赖默认 BPF 程序查找，也不接入生产 pin 路径。

验证依次包含：EPOLLIN|EPOLLOUT 的 edge-triggered 初始/后续事件观测；EPOLLIN-only 空闲等待；8 帧突发及部分排空后继续读 ring；poll 对照收包；耗尽 64 个 fill descriptor 并确认可观察的丢弃；归还 descriptor 后恢复收包；使用已归还用户的一个 frame 发送完整 TX，核对对端正文、CQ 地址，最后检查 RX 再次空闲。每档失败明确返回阶段、errno 和清理错误，不伪造空结果。三个独立进程循环复用同一对 veth，验证完整销毁后可以重新绑定。

`scripts/perf/afxdp-wake.sh` 要求 root 和与调用方不同的 network namespace。接口已存在时拒绝覆盖；仅创建、删除该次 namespace 中的 `ew-rx`/`ew-peer`，所有退出路径记录接口清理结果。测试 socket/UMEM/map/program 均无 pin，BPF detach 指定自己原先的程序 FD，避免移除不同程序。

## 本地验证与 CI 约束

C 源码已使用缓存的 libxdp 1.6.0 / libbpf 1.5.1 以 `-Wall -Wextra -Werror` 编译通过，workflow 和脚本的 Bash/YAML 语法检查通过。本地真实 user/network namespace 尝试在 `umem-create` 返回 EAFNOSUPPORT(97)，程序与接口均成功清理；这与第 4 轮能力结果一致，不能写成实际 ring 功能测试通过。

新增默认关闭的 `xdp_wake_probe=true` 输入，启用现有独立 XDP job 的真实 ring 探针。此 job 使用 Ubuntu 24.04 的 libbpf >= 1.0 API（发行版自带 1.3），库版本、源码摘要、namespace 身份、每轮结果及失败日志随产物保存。Rust correctness 与 splice 计时仍使用 Ubuntu 22.04 基线。

## 实际原生内核结果

提交 `81f970c5580140fd89ab248a4ea2b031b551b1d5` 的 [Actions 41531098471](https://github.com/Zelvik-Labs/edge/actions/runs/41531098471) 三个 job 全部成功：原生 XDP 行为探针、all-targets check / Clippy，以及常规 Rust 单元/集成与 vendored Pingora 检查/测试和 eBPF 构建。[实际 job 与 step 记录](evidence/af-xdp-wake-2026-11-02/ci-validation.json)。

原生 XDP job `125571192972` 的运行时内核为 `7.0.0-1017-azure`，libxdp 1.4.2 / libbpf 1.3.0。三个独立进程都完成 65 帧 RX、1 帧 TX 的完整正文和 CQ 地址校验，每轮 fill starvation 产生 1 个可观察丢弃，补充 descriptor 后恢复收包。每轮程序/socket/UMEM 的清理 errno 均为 0，脚本接口清理成功。

三轮都观察到 RW+edge-triggered 注册的初始事件为 `EPOLLOUT(4)`，后续空闲事件为 0。这不支持“永久可写本身就会让 edge-triggered 等待持续唤醒”的推论；也没有重现或解释旧 Tokio 注册的完整行为，现有代码注释中的归因需要独立核对。

8 帧突发中只排空一帧后，三轮的第二次 edge 事件均为 0；直接继续读取 ring 才取出余下 7 帧。这给后续 Tokio 实现留下明确约束：不能在刚取得就绪通知时就清除 readiness，必须持有它，直至在该 guard 创建之后实际观察到 RX 为空。Tokio 1.53.1 的 `AsyncFdReadyGuard::clear_ready` 文档也要求仅在观察到 WouldBlock 时清除，不能把一次成功读取当作已无就绪数据。

EPOLLIN-only 的 200 ms 空闲等待消耗约 36.4～40.9 µs 进程 CPU，这是短时诊断；三轮 `fill_kicks` 都为 0，恢复成功没有覆盖硬件 RX NEED_WAKEUP。完整 ring 功能结果只适用于本次 7.0 内核；6.8 内核在第 4 轮仅验证了 BPF 加载和 socket 创建。

产物 `13182066873` 为 3,194 字节、14 个原始文件，SHA256 `333e19df411857b2dfd269e18294aa7a5c777af40492e40c1a7546fc79cc9aea` 与 GitHub 元数据核对成功，C/脚本源码摘要也与测试提交匹配。[原始 ZIP](evidence/af-xdp-wake-2026-11-02/ci-raw.zip)、[逐文件校验](evidence/af-xdp-wake-2026-11-02/ci-archive-manifest.json)、[结果与库/内核信息](evidence/af-xdp-wake-2026-11-02/results.json)、[本地不支持与清理结果](evidence/af-xdp-wake-2026-11-02/local-unsupported.json)。

## 解释边界

veth SKB/copy 不能验证物理 NIC 零复制容量。copy 模式中的 fill recovery 也不能等同于验证硬件驱动的 RX NEED_WAKEUP；因此记录实际触发的 fill kick 次数。空闲 CPU 时间只是短时间观测，不是吞吐或稳定性结果。RW edge-triggered 控制用于核对实际事件，不能仅凭永久可写就宣称 Tokio 默认注册会忙循环；Tokio AsyncFd 生命周期、readiness clearing 和现有 poll worker 的成本仍需后续匹配对照。

本轮没有替换现有唤醒线程、没有修改 TCP/SNI 默认后端，没有批准或实施功能降级。当前产出是建立真实内核验证工具，尚不是可宣称性能提升的优化。RKE 保持暂缓。
