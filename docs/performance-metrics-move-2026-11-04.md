# 统计事件处理：消费已有维度所有权

优化后的实际 H1 HIT 采样中，维度统计处理约占 8.44% 的包含式样本，可见 Arc 析构与维度 key 哈希。
本轮先减少无意义的引用计数操作，不停用内置统计、不改变自定义投影范围或聚合格式。

worker 已独占收到的事件，旧实现却借用 RequestStats，再 clone geo、UA、WAF 维度供投影使用，
部分 geo 字符串还先 clone 成中间元组再 clone 成 ResolvedDimensions。投影结束又丢弃这些额外副本。
现在消费事件已有的分析结果，把字符串移入 ResolvedDimensions；自定义投影继续借用该结构，
调用结束后内置聚合再取得其所有权。category 和 waf_action 也直接转移。
分析/geo/provider 查询的触发条件、时间桶、事件计划、全部计数、排队字节退款与丢弃指标保持同一路径。

本轮不声称减少堆分配：Arc 的 clone/drop 主要减少原子引用计数，实际节点收益需要独立验证。
本地不编译 Rust，完整 CI 与 Edge 前后/Nginx/OpenResty 四目标 192 格对照将通过 Actions 执行。
[完整 CI 41769120615](https://github.com/Zelvik-Labs/edge/actions/runs/41769120615) 已通过并核验：1366 项库测试、16 项二进制测试、集成/vendor、check/Clippy，4 项库测试按既有规则忽略。
[日志来源与校验摘要](evidence/metrics-move-2026-11-04/full-ci/verification.json)已保留。此轮未重复原生 32 组 smoke，最近一次对应 c310 的可写等待修复。
[实际节点对照 41769127561](https://github.com/Zelvik-Labs/edge/actions/runs/41769127561) 正在运行：基线 e36710c、候选 e8d8b3a，强制仅 src/metrics.rs 生产源码不同，四目标、四协议、三缓存状态共 192 格，另有 128 项 Range 控制。
尚无新吞吐或稳定性结论，不能把减少引用计数的操作数当成节点收益。

仍未解决的节点整体差距、QUIC 客户端互操作错误与长期稳定性验证见总报告。
未修改 vendor、配置或统计采集范围。经审批的功能降级：无。
