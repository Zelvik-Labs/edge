# Native Quinn GSO：隔离此前偶发错误

运行时与测量脚本 `82e3cf0db722497fb594acdcfabc645a6deb2b56` 增加了显式 GSO 诊断，
默认仍为 auto。当前没有证据证明 GSO 导致或修复了此前两次 curl 56，也没有新的性能结论。

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
初次记录时工作流已启动，结果待补证。

两个条件复用[此前失败矩阵](performance-h3-native-2026-11-04.md)中的 32 KiB HIT 与 128 KiB OFF，
每个条件做四组交替顺序；每格 16,384 次新建连接，另有 5 秒连接复用对照。
共 32 格，计划 262,144 次新建连接，加上复用流量。两种模式都采用 matched 参数、显式 Retry off、相同 CPU 预算。
源站、正文、协议、缓存、连接数与 CPU/P99 检查继续生效；失败和未完成格不进入吞吐排名。

## 仍存在与设计限制

旧的两项错误仍然保留，根因尚未确认。即使本轮两个模式都零错误，也不足以宣布偶发故障消失。
关闭 GSO 可能增加大正文发送开销；该对照用于确定原因，不预先把关闭选项当成优化默认值。
只有两个历史条件，没有完整 H3 正文/网络矩阵、真实 NIC 或 24/72 小时稳定性结论。

## 经审批的功能降级

无生产默认行为改变；off 仅为显式、受限的性能诊断，转发内容与协议处理保持。

[运行及复核说明](../scripts/perf/quinn-gso/README.md)说明原始数据、实际可执行文件和公开 artifact 边界。
