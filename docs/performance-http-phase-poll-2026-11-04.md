# 实际请求阶段 poll 结果

[Actions 41802908107](https://github.com/Zelvik-Labs/edge/actions/runs/41802908107) 已通过显式 feature Clippy、release 构建、640 块/12,800,000 次测量、实际 ELF 与全部保留源码核对，零 Pending、零 host steal。源 61e525b85d021acb518af09f1f170132b796f317。默认构建不包含该 feature-only 诊断。

本次复用一个真实 TCP loopback 已解析 H1 GET、真实 ConfigStore/路由/LB/EdgeProxy、生产 socket digest 和 HTTP1 parse task scope，使用 mimalloc、当前线程 Tokio、x86-64-v2、release fat LTO/单 codegen unit。预热 1000 次，四种场景八轮轮换顺序、每轮二十块、每块两万次；每次实际方法必须到 Proxy 且保留 LB。计时后输出块，故失败可以保留部分结果。

## 四种场景

每块总耗时除迭代次数的中位数、最小/最大值，单位 ns/次。request-filter 包含创建/销毁上下文与生产 trait 方法，phase-polls 另有十个阶段计时及计数开销，两个数值不能用来推断代码版本变化。

| 场景 | 中位数 | 最小 | 最大 |
|---|---:|---:|---:|
| new-context | 185.37 | 181.87 | 195.81 |
| request-filter | 2387.73 | 2371.36 | 2657.09 |
| phase-polls | 3008.25 | 2979.01 | 3418.98 |
| clock-calibration | 62.58 | 62.44 | 68.93 |

## 实际私有阶段的 active poll 墙钟

所有十个阶段均实际执行，每阶段每迭代恰好一个 poll、没有 Pending 或早返回。下表包括每次 Instant/插桩及抢占开销，不是纯 CPU。空 ready future 的内部时钟校准中位数为 **28.78ns**，单独报告，不能作为精确可减常数。

| 阶段 | 中位数 ns | 最小 | 最大 |
|---|---:|---:|---:|
| context | 1234.91 | 1212.20 | 1399.31 |
| local_service | 36.71 | 35.82 | 46.03 |
| protocol_policy | 32.62 | 32.08 | 71.35 |
| early_response | 52.51 | 51.45 | 64.88 |
| security_state | 300.20 | 294.48 | 342.96 |
| limits | 501.28 | 493.56 | 572.72 |
| lightweight_waf | 62.11 | 60.79 | 77.16 |
| body_waf | 47.73 | 46.74 | 59.86 |
| feature_policy | 33.27 | 32.51 | 39.60 |
| rewrite | 33.39 | 32.83 | 45.45 |

context、limits、security_state 是该最小 GET 内的后续细查入口。body_waf 的大 future 帧并不意味着空 GET 执行成本大；本结果也不证明真实规则/请求体 WAF 便宜。构造/销毁成本与业务 poll 成本已有独立证据，不能混成一个百分比。

完整 request_filter 的本场景中位数约 2.39µs，没有测网络收发、完整响应或日志/维度事件提交。它不能解释全部节点 CPU 差距，也不能与不同 runner 的 CPU/响应数直接相减。futex 来源与额外写仍需 I/O/线程诊断。

## 证据与限制

归档 13309082781：7,374,425 字节、13 文件，SHA-256 `97117eb6849f9a9c276850360dd13eb8bb09df5c029ade980103a69d7e0ec1e3`。[冻结复核器](evidence/http-phase-poll-2026-11-04/pass/verify.py) 核对原始 ZIP、每个解压字节、实际 release ELF、源码、640 块顺序和阶段 poll 计数；[原始重算摘要](evidence/http-phase-poll-2026-11-04/pass/verified-summary.json) 保留各轮中位数和全部范围。

两个准备性运行 41802370237 / 41802390071 在源码审查修正后主动取消，未形成测量：前者新 example 未进入 sparse staging，后者缺实际 socket digest/生产 parse scope。正式结果不混入这些运行，也没有放松真实地址断言。

限制：同一已解析请求与最小配置被复用，没有源站 IO、缓存/响应/完整 logging 生命周期或 early_request_filter admission；不能外推不同协议、连接状态、真实 WAF 或节点容量。阶段独立中位数不可相加，插桩会改变布局和优化。未删除功能、未部署。

仍未解决：Edge 与 nginx/OpenResty 的整体差距。经审批的功能降级：无。
