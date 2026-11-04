# 主机名规范化的一次输出分配候选

请求过滤将主机名去端口、去尾点并转 ASCII 小写。旧路径的 strip_addr_port 先构造 String，再由 to_ascii_lowercase 构造另一个 String。
候选借用原始输入中的相同切片，只为最终规范化结果分配；未改变任何校验或增加可接受的来源。

原 helper 对括号 IPv6、单个/多个冒号、畸形方括号和非数字端口的行为全部保留；
userinfo 仍取最后一个 @ 后片段，Unicode trim、尾点和 ASCII 大小写转换顺序保持。
现有 String 入口计划保留并调用借用 helper；当前仅是 benches/ 隔离候选，未改变运行时。

不可变基线为 4ed5200，直接提取实际 strip_addr_port 和 normalize_request_host。
组件在 GitHub Actions 做 65,567 组确定性差分，同时核对原始去端口函数；包含 Unicode、控制字符、IPv6、畸形输入和长主机名。
十二场景、八轮平衡顺序、每轮二十块；分配与计时分开构建，System 分配器，保留全部退步和 steal。
正常非空主机名预期由两次输出分配变为一次，实际次数待探针确认；不从源码预报节点吞吐收益。

状态：等待 Actions。完整请求、缓存、WAF、统计和 TLS 的性能仍需真实节点验证。
已修复：本轮尚无运行时变更。仍未解决：整体用户态差距与长期稳定性。
设计限制：纯字符串组件不包含 Session/头表和生产 mimalloc 成本。经审批的功能降级：无。
