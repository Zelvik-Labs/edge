# Native Quinn GSO：隔离此前偶发错误

运行时与测量脚本 `82e3cf0db722497fb594acdcfabc645a6deb2b56` 增加了显式 GSO 诊断，
默认仍为 auto。实测确认关闭 GSO 仍发生错误，因此不能用关闭 GSO 修复该问题。

## 已实现

`ZELVIK_EDGE_QUINN_NATIVE_GSO=off` 只接受 native Quinn + matched profile。
空值、未知值和会被忽略的后端/配置组合明确报错；普通 shared Quinn、生产 profile 和 tokio-quiche 不静默套用该选项。
通过 Quinn 已有的 `TransportConfig::enable_segmentation_offload` 限制发送聚合，
没有增加自定义 socket wrapper、修改接收/GRO、拆包或读写就绪处理。
本仓库锁定的 quinn-proto 0.11.17 在关闭时将 `poll_transmit` 的最大 datagram 数设为 1。

配置回归覆盖合法值、非法值及后端/profile 组合。原生 listener 启动日志显示实际模式，
测量脚本逐实例检查该观测，不能仅凭传入了环境变量就算对照生效。

## 验证状态

[Actions 41714149785](https://github.com/Zelvik-Labs/edge/actions/runs/41714149785)
先执行全部 lib/bin 测试、全目标 Clippy 和 release 构建，再测试同一可执行文件的 auto/off。
工作流因真实传输错误返回失败，所有 32 个单元均已执行并保留：17 格通过、15 格出现传输错误。
全部源站参考零错误，最大宿主机 steal 为 0；同一可执行文件和逐实例模式日志均已核验。

两个条件复用[此前失败矩阵](performance-h3-native-2026-11-04.md)中的 32 KiB HIT 与 128 KiB OFF，
每个条件做四组交替顺序；每格 16,384 次新建连接，另有 5 秒连接复用对照。
共 32 格，完成 262,144 次新建连接尝试，加上复用流量。两种模式都采用 matched 参数、显式 Retry off、相同 CPU 预算。
源站、正文、协议、缓存、连接数与 CPU/P99 检查继续生效；失败和未完成格不进入吞吐排名。

## 实测与尚未修复的问题

总计 639,803 次成功响应、77 次错误；262,144 次新建连接中成功 262,067 次。
错误均来自 fresh 单元，客户端报告 curl 56、HTTP/version/body 为零，以及 OpenSSL `quic protocol error`。
复用单元全部通过。

| 条件 | auto：错误/尝试 | off：错误/尝试 |
|---|---:|---:|
| 32 KiB HIT fresh | 40/65,536 | 24/65,536 |
| 128 KiB OFF fresh | 10/65,536 | 3/65,536 |
| 合计 fresh | 50/131,072 | 27/131,072 |

错误数有差别，但该试验尚未定位 QUIC 关闭原因，也不证明 GSO 是唯一影响因素。
关闭 GSO 不能消除故障，不作为修复或新默认。失败 fresh 格不生成完整吞吐中位数。

复用控制中，关闭 GSO 的配对 RPS 中位变化：32 KiB HIT −21.15%、128 KiB OFF −13.23%；
CPU/请求分别 +78.51%、+63.02%。P99 同时下降（约 −25.17%、−55.05%），发生在不同的处理速率下，
不能只摘取延迟数字宣布整体改善。32 KiB auto 的客户端占用接近预算，也限制其容量解释。

[evidence](evidence/quinn-gso-2026-11-04/) 保存完整模式、四轮值、已核验记录及可移植复核器。
公开 artifact `13270484889`（27,179,167 字节、476 文件），实际二进制 artifact `13271188885`（19,388,352 字节）。
下阶段需要有界、显式开启的客户端/服务端协议诊断，捕获 transport close code 和相关帧；诊断吞吐不参与排名。

## 仍存在与设计限制

旧的两项错误与本轮 77 次错误均保留，根因尚未确认。未将协议错误改成重试成功。
关闭 GSO 可能增加大正文发送开销；该对照用于确定原因，不预先把关闭选项当成优化默认值。
只有两个历史条件，没有完整 H3 正文/网络矩阵、真实 NIC 或 24/72 小时稳定性结论。

## 经审批的功能降级

无生产默认行为改变；off 仅为显式、受限的性能诊断，转发内容与协议处理保持。

[运行及复核说明](../scripts/perf/quinn-gso/README.md)说明原始数据、实际可执行文件和公开 artifact 边界。
