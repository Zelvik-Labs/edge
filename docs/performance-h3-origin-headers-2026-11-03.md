# H3 回源重复响应头修复（2026-11-03）

本轮接续[回源连接复用修复](performance-h3-origin-reuse-2026-11-03.md)。源码为 `b5bc67ee7484915d7d6f86319cf239ff5d8ade38`，包含独立的测试导入修复 `473e519`。[GitHub Actions 41629076252](https://github.com/Zelvik-Labs/edge/actions/runs/41629076252) 的 Check/all-targets、Clippy、root lib/bin、integration 和 vendored Pingora 全部通过。

**修复了额外 Set-Cookie 和同名响应字段丢失；7 个真实 H3 回源测试及 1 个头转换回归通过。** 同时去掉字段分类时的临时小写 String 构造。本轮没有测吞吐、P99、CPU 或每请求总分配量。

## 已修复

锁定的 http 1.5.0 [HeaderMap owned iterator](https://github.com/hyperium/http/blob/v1.5.0/src/header/map.rs#L2003) 只在一个字段的第一个值返回 Some(HeaderName)，同名后续值返回 None，表示沿用此前的名字。旧 build_response_header 只处理 Some，因而丢掉后续值。例如源站发出 `Set-Cookie: a=1` 和 `Set-Cookie: b=2`，旧适配层只保留前者。

现在借用遍历全部名字/值，逐项 append 到 Pingora 响应头，保留两条 Cookie 以及同名自定义字段的值和顺序。HeaderName/HeaderValue 的浅克隆用于保留字段，不重新解析字段文本。连接专属字段过滤保持既有行为。

字段过滤函数现在接收 canonical HeaderName，直接匹配 as_str。HeaderName 已规范化大小写，避免请求和响应转换时再调用 to_ascii_lowercase 构造 String。代码消除了该临时构造，但未把它换算为整机性能收益。

另修复 harness=false UDP demux bench 引入 h3_ports.rs 时的 unused import：测试函数会被移除，测试模块的通配导入却仍存在。把导入放进各自测试函数后，完整 Check 日志不再包含该 unused import；保留两项原有全端口/快照测试，未屏蔽警告。

## 验证与证据

- 新增转换回归：两条 Cookie、两条同名自定义字段全部保留；混合大小写 Connection 输入及 Keep-Alive 仍被过滤。
- 真实 H3 源站在正常响应中发送两条 Set-Cookie 和两条 x-origin-value。完成响应的共同校验路径逐值断言，覆盖复用及上传等实际协议路径；204 仍验证无正文与干净结束。
- 原有顺序复用、128 KiB 上传双向进度、部分响应/RESET、放弃上传/cleanup、头及 trailers 超时、信任/group 隔离、空闲过期测试全部保留并通过。
- 七份 wire JSON 独立复算为 21 次已解析请求、10 条接受且关闭的 QUIC 连接；包括 6 次异常/放弃/超时控制，不能写成 21 次成功响应。顺序用例仍为 12 次请求/一条 QUIC/同一驱动。新增纯转换回归不生成 wire JSON；CI 日志另外记录其通过。
- 本地 Rustfmt 和 diff 检查通过；磁盘余量低于 6 GiB 启动门槛，本地未编译 Rust。全部编译和协议测试由 Actions 完成，工具链 1.99.1、x86-64-v2、jobs=2、test threads=2。

原始 artifact `13228395386` 为 3,292 B，7 文件/2,064 B，ZIP SHA-256 `d8e658fffb6361d1020378afb324148bc351fb332de0970daf59654ae5127d12`。原始 ZIP、每文件 SHA、独立复算器、源码摘要、8 项测试日志摘录和最终 jobs 状态保存在 [evidence/h3-origin-headers-2026-11-03](evidence/h3-origin-headers-2026-11-03/)。

在证据目录复算：

```bash
python3 unpack.py
python3 validate-wire.py wire wire-validation.json b5bc67ee7484915d7d6f86319cf239ff5d8ade38 41629076252
```

文档整合提交 `5371e08` 只合入上一轮报告和证据；逐路径 diff 确认 src/scripts/benches/workflow/Cargo 与测试源码 b5bc67e 完全相同。本轮未重跑 32 组 native suite；上一轮的结果绑定 28feef16，不计为本轮测试次数。

## 仍存在

H3 空闲池仍在每次借出/归还时全表扫描，活跃连接仍按请求独占；下一阶段需对高基数池开销和活跃流多路复用分别测量。当前结果没有证明超过 Nginx/OpenResty，也不能直接解释 H3→H1 的已有吞吐差距。既有大写二进制名的 manifest 提示仍存在，不宣称零警告。

## 设计限制

真实回源测试使用 Kernel loopback，不能代替真实网卡 AF_XDP、物理磁盘冷缓存或 24/72 小时稳定性。tokio-quiche 后端仍处于源码评估阶段；协议库、原生 UDP 入口和完整 CDN 路径需分别对照。多个 Cookie 的透传修复是协议正确性结果，不是业务缓存写入或持久化验收。

## 经审批的降级

无。未删除功能或放宽错误、缓存、协议及资源校验；未新增警告屏蔽、修改 vendor/upstream 或部署。
