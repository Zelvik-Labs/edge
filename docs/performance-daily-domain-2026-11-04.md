# 每日域名统计：事件域名所有权与现有 key 路径

真实 HTTP 采样中包含每日域名统计、分配与内存预算计算。本轮候选处理三个具体工作：
旧实现为每个事件分配 String，即使 key 已存在；每次都计算 MemoryGovernor 容量；现有 key 先 contains_key 再 entry，做两轮哈希/锁操作。

候选复用事件已有的 Arc<str>，现有 key 通过 get_mut 累积后返回；仅新 key 计算容量并尝试插入。
保留 record(&str) 的兼容入口与 String 输出，六个计数、非法域名拒绝、基数限制和可观测丢弃不变。
flush 时将共享域名转为输出 String；它的分配与新 key/兼容入口的代价需要分别评估，不能只展示热 key。

当前为 benches/ 下的隔离候选，未改实际统计路径。通过 GitHub Actions 编译、严格 Clippy、语义/并发控制及八轮配对计时。
原始方法取自不可变源码；DashMap 依赖与 root 锁定版本对应。
明确限制：组件 MemoryGovernor 使用固定容量 fixture，drop sink 是计数 fixture，不能把这部分耗时解释成实际预算快照成本。
计时用 System 分配器，分配计数用单独构建；不能据组件百分比预报生产 mimalloc 或节点收益。

20566 条检查包含全部计数、容量0/1/2/16/64、非法 server/域名、127/128字节、按天释放、四线程与并行 flush。
热 key 1/16/1024、达到上限的已有 key、新 key 拒绝、新增 key、非法输入和兼容入口分别测量；所有局部退步保留。
状态：等待 Actions。未修改 vendor、统计采集范围或资源上限。经审批的功能降级：无。
