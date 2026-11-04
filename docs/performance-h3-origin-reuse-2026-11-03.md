# H3 回源连接复用与生命周期修复（2026-11-03）

本轮修复 HTTP/3 **回源**连接池，而非替换下游 H3 后端。源码为 `28feef16bbb39b11d3701fbbac1959c608bfa9e5`，基于上一轮报告提交 `e1c35a0`。验证来自 [GitHub Actions 41623661117](https://github.com/Zelvik-Labs/edge/actions/runs/41623661117)。

**7 项真实 H3 回源回归通过；12 次顺序请求实际复用同一 QUIC 连接和 H3 驱动。** 全部 21 次已解析请求包含 6 次异常、放弃或超时控制，不是 21 次成功响应。10 条接受的 QUIC 连接均已关闭。独立复算校验了原始 ZIP、七个文件及全部计数。

32/32 组隔离 native 功能套件通过并独立复算：64 项 H2 检查、128 次 H3/QUIC 探针、128 个原始坏帧计数、128 次非法应用回复超时、96 次有效报文正控制；32 组均完成重载、队列归零和网络清理。Copied/Owned × Eager/Cooperative × 8 种 wake/statistics 组合全部保留。

## 已修复

原实现建立请求后始终保留 `request_stream=Some`，复用判定却要求其为 `None`，正常完成的请求无法进入连接池。此外，池只保存 Quinn Endpoint/Connection；当前锁定的 h3 0.0.8 在最后一个 SendRequest 释放时会关闭连接。单纯清空请求流仍不足以保活；在旧 QUIC 上再次创建 H3 驱动也会重复创建协议控制流。

现在每条 QUIC 连接拥有一个共享 H3 上下文，保留 Endpoint、SendRequest 和一个有生命周期归属的驱动任务。借出和归还请求复用该上下文；最后一个拥有者释放时关闭 QUIC 并中止驱动。

原请求流的同一 Mutex 同时包住上传和读取响应头。Pingora custom upstream 并发执行这两个方向，响应头等待可能阻塞上传，而源站又在等待上传完成。现在拆分发送与接收半流；BodyWriter 与会话共享成功 FIN 和失败状态。仅在请求 FIN 成功、最终响应头有效、响应 DATA 及 trailers/FIN 全部完成且连接仍开放时入池。放弃上传、RESET、部分响应、超时和实际结束错误均禁止复用；归还时的结束错误明确记录。

其他边界处理：

- 池键包含目标/SNI、Peer 复用哈希、证书/主机名验证策略、实际 AF_XDP registry 身份，以及已解析的传输策略。连接方案一次捕获运行时快照和 registry；需要 AF_XDP 却缺少 registry 时明确失败。
- 保留 loopback 使用 Kernel socket 的既有行为，以及既有传输控制器选择语义；未增加隐式 Kernel 回退。
- 空闲池每条记录持有 semaphore permit，最多 4,096 条；替换不多占名额，取出或清除释放名额。空闲 TTL 仍为 120 秒，显式内存回收入口保留。
- trailers 读取遵守 read timeout；发送结束错误不再被忽略。TLS 显式选择既有 aws-lc/TLS 1.3，报告本机 CA 加载和拒收错误。
- 响应数据使用 Buf::copy_to_bytes，去掉适配层强制新建 BytesMut 的复制步骤；没有据此估算吞吐或总分配收益。

## 验证与证据

回归使用现有测试证书、真实 Quinn/h3 loopback 源站、Kernel 策略和生产 OriginH3Connector。测试作用域在退出或 panic 时恢复此前完整的运行时配置。每个源站停止后等待处理任务，并核对 accepted/closed，再写出证据。

| 实际用例 | 已解析请求 | 接受/关闭 QUIC | 关键断言 |
| --- | ---: | ---: | --- |
| 顺序复用 | 12 | 1 / 1 | 包含 204、正文及 trailers；stable_id 和 driver id 保持相同；名额归还 |
| POST 双向进度 | 1 | 1 / 1 | 128 KiB 上传，源站收完正文才回头；并发等待头与上传；重复 FIN 幂等 |
| 部分响应及 RESET | 2 | 2 / 2 | 不能归还为健康连接 |
| BodyWriter 放弃及 cleanup | 2 | 2 / 2 | 未完成上传不能在 release 时变成健康条目 |
| 响应头及 trailers 等待超时 | 2 | 2 / 2 | 实际 ReadTimedout；不把 trailers 尚无 FIN 当完成 |
| 信任和 Peer group 隔离 | 1 | 1 / 1 | 不安全或其他 group 连接不可被严格策略借用；原策略仍可复用 |
| 空闲过期 | 1 | 1 / 1 | Sender/driver/endpoint 生命周期结束，远端观察到关闭 |
| 合计 | 21 | 10 / 10 | 含异常控制，不是完整成功响应计数 |

Actions 使用锁定工具链 1.99.1、x86-64-v2、jobs=2、test threads=2。Check/all-targets、Clippy、root lib/bin、integration 和 vendored Pingora 测试通过；真实回源测试属于 test/debug 配置，不是发行性能测量。本地仅做语法/格式和脚本检查：可用空间低于 6 GiB 启动余量，未进行本地 Rust 编译，也未清理构建缓存。

证据位于 [evidence/h3-origin-reuse-2026-11-03](evidence/h3-origin-reuse-2026-11-03/)：原始归档、GitHub 元数据、逐文件 SHA、独立复算器、复算结果、源码摘要和最终任务状态。回源 artifact `13224688996` 为 3,292 B、7 文件/2,064 B，ZIP SHA-256 为 `0a410f4c07cd3e02ffb34d2b226029fed249551b430ba2a9379e75cb35503eee`。工作区离线期间旧临时下载引用失效，首次请求返回 403、未收到归档字节；更新引用后下载成功。失败记录保留，无私有下载 URL 入库。native artifact `13227935644` 为 137,214 B、69 文件/520,061 B，ZIP SHA-256 为 `3fe769363c7f24ecf040c63b593e8b1598ced855cad1455361d6715d3354d5b9`。原始日志保留在 ZIP；不另外提交可由归档逐字恢复的日志副本。

复算命令在证据目录执行：

```bash
python3 unpack.py
python3 validate-wire.py wire wire-validation.json 28feef16bbb39b11d3701fbbac1959c608bfa9e5
python3 validate-native.py native/extracted native-validation.json 28feef16bbb39b11d3701fbbac1959c608bfa9e5
```

## 仍存在

- 本轮未测吞吐、P99、CPU/请求或 RSS，不能宣称超过 Nginx/OpenResty。已有主要 H3 容量比较使用 H1 源站；H3 回源池修复不直接证明这些 H3→H1 场景提速。需新增 H3→H3 独立场景，并重跑同条件 HIT/MISS/OFF、复用/多路复用/新建连接比较。
- 空闲池 take/offer 仍分别全表扫描，成本随空闲键数增长；当前借出时移除条目，并发请求不会共享正在使用的 QUIC 连接。清理调度及活跃连接多路复用需要单独测量和实现。
- 复核另发现原有响应头转换丢失同名后续值，包括额外 Set-Cookie。本轮冻结版本仍有该问题；独立后续候选 `b5bc67e` 已修复，Actions `41629076252` 验证中，不把后续修复追记为本轮冻结版本的行为。
- Check 有既存 harness-free benchmark 的 unused import 警告，后续独立提交 `473e519` 调整测试函数内导入；既有大写二进制名称的 manifest 提示仍保留。本轮没有加入任何警告屏蔽，不宣称零警告。

## 设计限制

回源证据是 Kernel loopback；native suite 是隔离 veth/SKB/copy 功能验证，其 loopback 源站也使用 Kernel 回源。它们不代表真实网卡 AF_XDP 回源、硬件零复制、4,096 连接并发饱和或 24/72 小时稳定性验收。空闲池上限不等于全部活跃回源连接上限。

仍使用 h3 0.0.8、h3-quinn 0.0.10、Quinn 0.11.11。tokio-quiche 尚未接入；候选的原生 UDP、动态 BoringSSL 证书、共享 UDP/AF_XDP 入口与资源边界要求见[迁移评估](performance-h3-tokio-quiche-evaluation-2026-11-02.md)。后续保留 Quinn 共享入口、Quinn 原生入口、tokio-quiche 原生入口三方控制，分别识别入口架构和协议库影响。

## 经审批的降级

无。未关闭协议、缓存校验、统计、准入、队列记账、超时或错误路径；未修改 vendor/upstream，未接入 RKE、发布或部署。
