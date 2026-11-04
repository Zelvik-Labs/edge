# 第 9 轮：健康队列门禁与逐轮分配

修复内部 AF_XDP 门禁在**全部配置队列已故障**时仍返回 true 的问题，并移除 bridge 每轮创建接口 `Vec` 的操作。保留健康队列继续工作和原有公开状态定义；本轮没有测量这处修改的吞吐收益。

源码 `2f65ad03d1ed309b62746b5d55363b75b848ac71` 位于独立分支 `perf/queue-health-20261103`，[Actions 41564288724](https://github.com/Zelvik-Labs/edge/actions/runs/41564288724) 两个执行 job 均成功，含完整 Rust、Clippy、Pingora、eBPF 门禁与四轮真实节点检查。第 8 轮 UDP 数据测量的是此前的 `4b29028`，不能将其结果归给本修复。

## 已修复

`proxy_xsk_ready` 原本把故障且已退出的队列视为“已处理”，目的是保留健康 siblings，但没有要求至少一条健康队列。因此最后一条健康队列退出后，内部 redirect/worker 继续条件仍可返回 true。新回归在该状态下得到 `true`、期望 `false`，复现了谓词错误；没有据此声称复现真实 NIC 故障或实际丢包。

新实现借用配置逐项检查：空队列、缺失状态或未就绪且未退出的队列仍拒绝；已退出队列不阻塞健康 siblings；至少有一条配置内健康队列才能通过。无关接口的 ready 状态不能让全故障配置恢复就绪。内部队列状态按 `(interface, queue)` 维护唯一条目，仍在同一读锁下检查。

`proxy_bridge_should_continue` 每轮调用此条件。原先 `collect::<Vec<_>>()` 在有效接口路径逐轮分配，新实现直接迭代，没有新增锁、状态缓存、配置字段或动态派发。移除分配是源码事实，尚不能换算为节点吞吐提升。

公开 `status.proxy_ready` 仍要求全部配置队列健康。本轮首次测试误以该公开值观察内部部分存活条件，第一条队列退出就失败；更正观测对象后，才得到最后一条健康队列退出时的正确失败复现。两次失败日志都保留，第一份不作为零健康队列问题的证据。

## 验证

本地相关 **9 项测试通过、0 失败、0 忽略**，包括既有 siblings、生命周期与新回归；全目标 Clippy 通过。格式检查发现原有无关差异，仅修正新代码格式，不声称全仓格式干净。

Linux **6.8.0-1070-azure** 上，以同一对节点/H3 客户端二进制，按 worker/readable/readable/worker 顺序完成四轮套件，均成功。覆盖 IPv4/IPv6、TCP 半关闭、H1、HTTPS、H2、SNI、UDP、H3、QUIC、raw redirect/bypass、未匹配内核流量及 detach。

独立提取并核对 **8 次 H2 检查和 16 个 H3/QUIC 成功探针**，没有跳过 H2。每轮应用后端记录 HTTP 2、HTTPS 4、TCP 2、UDP 2、H3 2、SNI 2、QUIC 2 次请求/连接。所报队列故障、丢弃、环满、无效描述符、拥塞/AQM 丢弃、准入拒绝和残留 TX 均为 0。

四次 reload 都实际替换 manager，bridge supervisor 保持存活，前后 ready 检查通过。每轮退出仅剩私有命名空间的 lo，无命名 netns 残留；前后二进制摘要一致。功能套件墙钟时间不作为吞吐或尾延迟数据。

## 证据

原生产代码与测试差异只有 `src/xdp/mod.rs`、`src/xdp/tests.rs`。后续的报告、负载源码原样迁移及工具路径更新没有再修改这两处运行时代码；迁移后的 C 工具另行重编，二进制逐字节相同，5 项本地工具测试通过。

原生产修复的本地归档为 7 文件、6,705 字节。CI 产物 `13199965767` 为 **18,025 字节、12 文件**，SHA256 **`ffd6c2fb3b81745bdb5471d3b2e37be4d1fa14474183cb5adcd420ef4eaaa3e4`** 与 GitHub 元数据一致。

- [本地失败、通过与 Clippy 原始记录](evidence/af-xdp-queue-health-2026-11-03/local-validation.zip)、[本地清单](evidence/af-xdp-queue-health-2026-11-03/local-manifest.json)。
- [CI 原始产物](evidence/af-xdp-queue-health-2026-11-03/ci-final-raw.zip)、[逐文件摘要](evidence/af-xdp-queue-health-2026-11-03/ci-final-manifest.json)、[产物元数据](evidence/af-xdp-queue-health-2026-11-03/ci-final-artifact.json)。
- [独立校验](evidence/af-xdp-queue-health-2026-11-03/ci-final-validation.json)、[提取的协议/reload 报告](evidence/af-xdp-queue-health-2026-11-03/ci-final-reports.json)、[环境摘要](evidence/af-xdp-queue-health-2026-11-03/ci-final-metadata.json)、[汇总](evidence/af-xdp-queue-health-2026-11-03/ci-final-summary.json)、[job/step](evidence/af-xdp-queue-health-2026-11-03/ci-final-jobs.json)。

## 未解决与设计限制

零健康队列的回归是受控 manager 状态测试，尚非原生 FD/线程资源耗尽注入。启动 ready 消息早于 wake 初始化的窗口仍需单独验证。现有 smoke 的 TCP 重试保留，因此通过不证明每次首次尝试都成功。

本地宿主不支持 AF_XDP，原生验证来自 CI 的 veth SKB/copy；真实 NIC、零复制、硬件 NEED_WAKEUP 和长稳尚未验证。H3 库没有替换，默认 worker 不变，RKE 暂停。下一轮优先解决 bridge 临时统计触发的重复解析/分配，保留公开统计和全部校验，以同二进制做匹配对照。

## 功能降级

没有申请或实施功能降级，没有发布、部署或合并 main。编译缓存保留；历史 perf 文件无损归档后的原始字节、SHA 和恢复命令仍在，6/5GiB 空间保护没有绕过。
