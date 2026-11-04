# 下一阶段性能工作：源码同步与多协议验证

日期按用户时区 Asia/Hong_Kong。用户后续要求暂不考虑 RKE，已转向本地实现：指标提取依赖与模板编译已经落地，另修复两处指标漏记。见[实现、回归与组件实测](performance-extraction-implementation-2026-11-02.md)。后续已补充[生产启动的协议、缓存与连接状态对照报告](performance-protocol-state-comparison-2026-11-02.md)；其中保留状态不成立、错误和超时，长期稳定性尚未验收。

架构诊断后的第一批生产改造和独立前后对照见[架构优化实现报告](performance-architecture-implementation-2026-11-02.md)。已处理请求重复准备、小对象存储压缩、文件任务与 H2 正常轮换；完整紧凑数据面、worker 本地统计、H3 缓存与存储资源隔离仍在后续范围。下文保留最初接续时的历史状态，当前构建与回归以新报告的冻结二进制和源码摘要为准。

继续推进的 L1 插入校验、实例指针、类型化头查询、回源参数预构建和统计分析依赖见[第二批优化报告](performance-architecture-stage2-2026-11-02.md)。新版的新连接与复用结果分表记录；后续状态以该报告为准。

## 接续基线

- Edge：`01a2156882789f876f3c40c01bc6a0ee0bc5fa2b`，独立工作树 `.worktrees/perf-next`、分支 `perf/next-20261102`。
- docs：`d8a1541`，已 fast-forward，按新的云接手、执行规则与集群边界操作。
- control-plane：`b5e3ecc6443315b27a4d356bb92f48348b944c6c`，已 fast-forward。
- 原 Edge 工作树的未提交优化和历史测量保留，没有把旧实现直接覆盖到新版本。

新版本已经实现新的指标计划、投影、配置代退役、分钟窗口、耐久 revision 交接与精确 IP 短批次；此前自定义聚合器不能整体搬回，否则会覆盖新契约。下一步优化必须以当前实现为基线，保持关闭自定义指标时不采集、基础统计继续记录，以及配置热更新和耐久交接守恒。

源码核对也确认 H3 通过 `H3DownstreamSession` / `ServerSession::new_custom` 接入共享代理链，不能仅因旧字段 `is_http3_bridge` 就认定存在回环 TCP 转发。协议差异需要分别测量和采样。

## 已完成验证

最新库通过共享 `local-rust.sh check --locked --lib`（内部 mbx、jobs=2、单编译槽），完成用时 4m48s。这是库检查，不是生产发行构建或全仓测试。
第一次检查被新的 12 GiB mbx 总预算中止；按预算回收旧对象缓存后重新检查通过，受管 target 和增量目录未被清空。没有放宽预算或绕过监视器。

新增 `scripts/perf/protocol_matrix.py` 与说明，独立验证明文 H1、TLS H1、H2 和 H3。校验证书、实际协议、HTTP 200 和完整正文；降级、失败和不支持分别记录。原生容量模式只允许单个目标，按随机种子顺序重复，并记录工具/驱动 SHA、宿主 CPU tick 和原始结果。

四项行为回归覆盖正常 H1 与正文损坏、HTTP/1.0 降级、失败端点不冒充不支持且不阻断其他独立校验，以及禁止多目标原生容量任务。真实 Nginx 本地 fixture 已通过四种协议各三次的完整性校验，含 H3 UDP。这些结果验证测试工具，不代表最新版 Edge 已通过协议或性能验收。

原生客户端的补测发现：现有 OHA 1.16.0 未编译 HTTP/3，且旧 fixture 证书直接使用 CA 作为服务端证书，被 Rustls 正确拒绝。另有到期后未等待在途请求引起的截止错误；已为工具增加 `-w`。这些失败均保留在本地证据，不能归为服务端“不支持”，也没有禁用证书验证。

已通过共享 mbx 包装器构建 OHA 1.16.0 release + `http3`（11m23s），原有客户端未被覆盖。采用独立 CA / 服务端证书并等待在途请求后，最终四协议原生调用路径全部通过，共 58,049 次 HTTP 200、零原生错误。H2 固定 `-p 1`，每种协议仅 2 秒，使用本地静态 Nginx fixture，因此只证明调用链可用，不是代理容量排名。最终 manifest 的驱动 SHA 与当前文件一致，保留了每次命令和 stderr。

证据：[行为测试](../.local/perf-next/protocol-tests.log)、[原生客户端构建标识](../.local/perf-next/oha-http3-build-metadata.json)、[最终四协议校验](../.local/perf-next/native-client-smoke-validated/manifest.json)、[证书身份错误负向校验](../.local/perf-next/negative-tls/manifest.json)。

本地原始记录在 `.local/perf-next/`；测试 CA/私钥只在该忽略目录，不进入 Git。

## RKE 连接的历史检查（当前暂停）

用户已授权使用更新后的云环境，但会话仍绑定 revision 1：没有运行期身份，`vpn_configured=false`，没有 TCP grant；当前用户不存在已安装的 SSH/kubeconfig 目录。
经继承的 HTTPS 代理实际访问已登记 RKE API 的 `/version` 时，证书身份不匹配，curl 返回 60、HTTP 000。没有建立可信的 Kubernetes 会话，也未关闭 TLS 检查。

当前可调用工具只有环境状态与诊断，没有配置切换接口。授权不等于环境已经重新绑定；需要在任务界面实际选择配置好网络和运行身份的环境，再重查 readiness、SSH host key 或 kubeconfig 身份和真实节点状态。不得通过改路由、猜凭据或复制他处身份解决。

## 后续完整节点验收

1. 当前使用本地隔离测试继续推进；恢复 RKE 后再检查 context、`zelvik-dev` 配额、CI 节点标签和 CPU/内存余量。集群访问不作为源码实现的前置条件。
2. 构建同版本 Edge release 基线与当前候选，记录源码、依赖、编译参数和镜像 digest。旧 C2 二进制不能充当 26.11.1 的基线。
3. 先校验 H1/TLS H1/H2/H3 协商与内容，确认原生客户端对应能力。HTTP/3 客户端必须在集群内运行，TCP/SSH 隧道不能承载 QUIC。
4. 按 README 的响应体/并发矩阵，逐目标独立交叉测试；同步 CPU/请求、P99、内存/FD、队列和指标计数。steal 或同镜像波动过大则不验收稳定提升。
5. 指标提取与模板的配置编译执行计划已实现。连接缓冲复用、头部借用仍需依据新版本完整节点 CPU/分配栈继续推进；先通过等价性、热重载、慢客户端、断连、重试和统计守恒，再比较性能。
6. 通过短测后连续 24–72 小时验证，再报告真实领先范围。没有通过的项目继续列为未完成，不以简化转发原型替代完整 Edge。

本轮没有生产发布、集群写入或新增功能降级。
