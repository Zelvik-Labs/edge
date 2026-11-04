# 实际 HTTP 回调 future 的 release 布局

当前节点采样中，请求过滤和回源流水线仍有较大成本。本轮直接量化六个实际 ProxyHttp 回调的堆上 future 状态大小，
作为后续调整异步结构的依据；不从功能数量或源码长度推断它们占了多少 CPU。

probe 链接实际 EdgeProxy，以解析过的本地 HTTP/1 会话与默认上下文构造各回调，读取动态 future vtable 的具体布局后销毁。
所有回调均不 poll，不执行 WAF、缓存准入、回源或日志，不对真实用户请求调用这些函数。
使用 production 相同的 mimalloc、固定 Rust 版本与 x86-64-v2 release 构建，保留实际 probe、源码和二进制哈希。

这是结构测量，不是 CPU/耗时、分配次数、分配器实际块尺寸或节点容量。小 frame 仍可能执行很多工作，
大 frame 也不能单凭尺寸解释吞吐差距。只有在看到具体布局后才选择并测试后续结构改动。

实际 [CI 41774460518](https://github.com/Zelvik-Labs/edge/actions/runs/41774460518) 的 release probe 已完成并核验实际可执行文件，完整节点 smoke 仍在进行。

| 实际回调 | future 字节数 | 对齐 |
|---|---:|---:|
| early_request_filter | 32 | 8 |
| request_filter | 5736 | 8 |
| upstream_peer | 32 | 8 |
| upstream_response_filter | 40 | 8 |
| response_filter | 40 | 8 |
| logging | 40 | 8 |

request_filter 显著更大；它依次 await 十个阶段，状态对象必须容纳可能挂起的分支。
现有大小可指导后续检查，但不能单凭 5736 字节断定它占了多少 CPU、每请求触碰多少缓存行或分配器实际块尺寸。
下一轮增加显式 ignored 的 release 布局探针，分别测十阶段与状态响应 helper，同时与实际回调整体大小交叉核对。
探针不 poll 任何业务阶段；在得出分支尺寸与真实测量前不盲目给所有 await 加 Box。

归档 `13297500892`：2,292,617 字节、4 文件，SHA-256 `39c2732dfbb46d494ccd855146a3dfffff37b916e7ed124b82ee2a1f5a8e24d2`。
[冻结复核器](evidence/http-future-layout-2026-11-04/verify.py)检查全部归档、固定源码 SHA-256 和实际 release probe 的 SHA-256（`8902546aa03c533476cef87a982958cb9d7244f37903f31130b914a594a79db8`）。
生产回调实现保持，未移除功能或修改 vendor。
经审批的功能降级：无。
