# AF_XDP 自动绑核：按实际 CPU 身份选择

修复了自动队列绑核把“可用 CPU 数量”当成“从零开始的 CPU 编号”的问题。
原生控制已重现：继承 `taskset -c 1,3` 的旧进程把自己重新绑定到 CPU 0，新版选择 CPU 1。
这是 CPU 放置正确性修复，没有据此宣称 QPS 提升。此前 UDP 对比显式配置了 CPU，因此不能用这个 bug 解释那份性能差距。

运行时 `a5098908d49a8610be9654e176b4a22d0efba10f`；
[Actions 41710360453](https://github.com/Zelvik-Labs/edge/actions/runs/41710360453) 已通过。
后续累计版本 `fe89411` 的[完整 CI 41712030753](https://github.com/Zelvik-Labs/edge/actions/runs/41712030753) 也已通过：
1360 项库测试、16 项二进制测试、集成/vendor、check/Clippy 以及 32 项完整 AF_XDP 节点组合。
[完整节点证据](evidence/range-preflight-2026-11-04/full-ci/)已校验来源、文件哈希、运行后清理及测试摘要。

## 已修复

启动时读取一次 supervisor 的 `sched_getaffinity`，按实际 CPU ID 轮询分配队列。
显式 `interfaces[].cpus` 保持优先，不做静默重映射。动态位图允许内核 CPU 集合超过 libc 固定 mask 大小，
限制查询缓冲不超过 1 MiB；无效 CPU index 明确返回错误，不执行越界位操作。
绑定失败仍保留既有警告和继承亲和性行为，并增加 OS 错误详情。
无法读取亲和性时退役尚未启动的 lease，通过既有可观测的 dataplane 失败路径关闭 redirect，避免出现无 worker 的假就绪状态。

| 真实进程控制 | 继承 CPU | 自动选择 | 调用后 CPU | 断言 |
|---|---|---:|---|---|
| 旧版观察 | 1,3 | 0 | 0 | 保留实际结果 |
| 旧版“不得离开继承集合” | 1,3 | 0 | 断言先失败 | 预期退出 101 |
| 新版观察 | 1,3 | 1 | 1 | 通过 |
| 新版集合约束 | 1,3 | 1 | 1 | 通过 |

enclosing cgroup 允许 0～3，所以上表是扩大自身 taskset 范围，不是越过 cgroup 的硬限制。
父进程的 CPU 集合不变。两项生产单元测试覆盖稀疏 ID、跨 word 位图、无效 ID，
以及单独线程的实际绑核后读回；旧/新选择器都检查显式配置优先。

## 仍存在与设计限制

探针只验证 queue zero、CPU 身份和配置优先级；旧计数由 fixture 提供，queue zero 对任意正数计数都会选 0，
因此该负控制不依赖 CPU quota 的具体计算。没有执行 AF_XDP 收发、测 NUMA 放置或真实 NIC 吞吐。
自动选择是启动时快照；运行中 cgroup/taskset 更改及 CPU 热插拔需要独立生命周期设计。
显式配置在 OS 不允许时仍按原行为报告警告，没有把配置错误隐藏成另一个 CPU。

## 经审批的功能降级

无。协议、转发和统计行为不变；没有用少启动队列提高成绩。

## 复核

artifact `13267217960`，273,711 字节，SHA-256
`706600a39fddb5afaf382c35f86ce467fed467f69d1bb3558c93dd3246f47d11`。
全部 15 文件、生产函数快照、实际可执行文件、四项控制与测试日志均已核对。
二进制保留在 Actions artifact；[仓库证据](evidence/affinity-2026-11-04/)含结果和可移植校验器。

```sh
python3 docs/evidence/affinity-2026-11-04/verify.py /path/to/artifact.zip
```
