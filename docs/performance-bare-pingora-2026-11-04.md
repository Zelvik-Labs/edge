# 纯 Pingora OFF 回源架构对照

现有 Edge/竞品对照已经证明普通请求仍有差距，但仅靠 Edge 单侧热点无法区分框架与业务成本。
本轮增加独立的纯 Pingora 诊断服务器：相同 vendored Pingora 0.9.0、Rustls/AWS-LC、mimalloc、x86-64-v2、fat LTO和单个代理worker。
只做H1/H1-TLS/H2 OFF回源，预制HttpPeer后克隆复用；不包含Edge的控制面、WAF、缓存、业务统计或内存治理。
这是功能受限的对照程序，不替换实际Edge，不作为生产功能等价成绩。

Edge仍使用不可变ba474实际已测二进制，控制器/客户端也用固定SHA-256校验的既有文件。
构建前断言当前vendor与Cargo.lock相比该Edge版本未变；全部共享registry版本/校验和匹配root锁文件。
新程序不依赖Edge业务库，避免现有bench-proxy启动缓存及其后台任务污染框架基线。

四目标Edge/Pingora/Nginx/OpenResty、三协议、四轮平衡顺序，共48格；每格全新实例、1KiB、32并发、5秒、复用连接。
代理/客户端/源站独立1/2/1 CPU，代理2GiB；核对逐响应协议/状态/长度、前后正文SHA-256与TLS、OFF逐请求回源，以及32项错误CA拒绝控制。
同一批源站stub_status查询另外记录accepted/handled/requests，核对上游连接复用，避免把前端复用误当成回源也复用。
同时读取稳定host PID的/proc/stat进程计时：Edge主进程与测试控制器分开列出，避免把测试用控制器CPU全部解释成Edge业务成本。
该观察只在本轮Linux host PID namespace中显式启用；按USER_HZ计时有量化误差，与cgroup边界不要求逐微秒相等，亦不能直接从吞吐中减掉控制器比例推容量。

解释结果时同时检查客户端、源站与代理预算，报告CPU/有效请求及P99，不只看RPS。
框架基线的worker为1、上游idle超时120秒、池容量256；Edge池容量由原内存治理决定，两者均大于本轮32并发。
业务范围和后台任务不同是本轮需要量化的变量；即使基线更快，也不能单独证明可以删除哪个模块而保留全部业务。

首轮[Actions41786924510](https://github.com/Zelvik-Labs/edge/actions/runs/41786924510)在构建时失败，尚未发出测量请求。
独立manifest没有启用Tokio文件IO，但当前vendor包含tokio::fs调用；主Edge通过full功能已有该依赖。
现由独立manifest显式启用同版本Tokio/full，不改vendor，不关闭IO功能。
失败归档13301319318保留7文件、59,911字节，SHA-256 `4efe93249c3c9ec22ae7e87183b02aabdc636703b9f57c33414506166e4d4ec1`。
状态：修复后将重新执行Actions，未给出任何性能结论。
已修复：本轮增加诊断能力。仍未解决：业务/框架成本分摊与整体超越目标。
设计限制：固定旧版Edge以控制vendor一致；HIT/H3不属于此基线，不能扩展结论到它们。
经审批的功能降级：无；实际Edge功能保持。
