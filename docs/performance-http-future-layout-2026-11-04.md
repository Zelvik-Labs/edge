# 实际 HTTP 回调 future 的 release 布局

当前节点采样中，请求过滤和回源流水线仍有较大成本。本轮直接量化六个实际 ProxyHttp 回调的堆上 future 状态大小，
作为后续调整异步结构的依据；不从功能数量或源码长度推断它们占了多少 CPU。

probe 链接实际 EdgeProxy，以解析过的本地 HTTP/1 会话与默认上下文构造各回调，读取动态 future vtable 的具体布局后销毁。
所有回调均不 poll，不执行 WAF、缓存准入、回源或日志，不对真实用户请求调用这些函数。
使用 production 相同的 mimalloc、固定 Rust 版本与 x86-64-v2 release 构建，保留实际 probe、源码和二进制哈希。

这是结构测量，不是 CPU/耗时、分配次数、分配器实际块尺寸或节点容量。小 frame 仍可能执行很多工作，
大 frame 也不能单凭尺寸解释吞吐差距。只有在看到具体布局后才选择并测试后续结构改动。

实际 [CI 41774460518](https://github.com/Zelvik-Labs/edge/actions/runs/41774460518) 的 release probe 已完成并核验实际可执行文件；完整1367库测试、集成/vendor/lint及32组节点也已通过并核验。

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

## 大 frame 的实际构造成本

补充与阶段尺寸独立的实测：同一实际 EdgeProxy、production mimalloc，每个回调二十块、每块十万次构造/销毁，合计120块与1200万次。
使用 black_box 保留 future 的可见地址，所有 future 仍不 poll；Unix 会话、默认上下文与响应 fixture 在计时前构造。
编译结束后绑单核执行，记录宿主 steal 与全部块，保留实际 probe 二进制和源码。
这能检验大 frame 是否连构造都很昂贵，不能覆盖首次 poll 初始化、业务执行或挂起/唤醒成本，也不直接计算节点可回收比例。
在有这组绝对耗时之前，不把 5736 字节当成性能差距的根因。[诊断Actions41779412321](https://github.com/Zelvik-Labs/edge/actions/runs/41779412321)已启动；该工作流只验证诊断构建/Clippy和测量，不冒充完整功能CI。

该诊断现已通过并核验实际二进制、全部120块、1200万次测量，零steal。request_filter构造/销毁中位29.22 ns（27.35～54.86），其他五个约5.93～10.86 ns。
这是纯构造/销毁的绝对耗时；大frame本身尚不足以解释几十微秒的节点CPU成本，不能据尺寸把所有阶段改成额外堆分配。
首次poll、阶段执行和挂起/唤醒仍未被这项组件测量覆盖。
归档13298538441：6,871,255字节、9文件，SHA-256 `faaf8d25f8cb69dde91611408ffde642cd4dfa484b62777354b24bd05ce2ed5c`；[冻结复核器](evidence/http-callback-construct-2026-11-04/verify.py)保留全部原始块。

## 阶段测量中的构建差异

41777560016的常规测试、集成/vendor与check/Clippy通过，但布局步骤的等同断言失败：生产release回调5736字节，release测试库回调5808字节。
测试库中body_waf为5672字节，明显大于其他阶段（context2560、protocol_policy2344）；该ignored探针实际1项通过。
测试库含cfg(test)/测试harness，与生产构建不同；未单独证明72字节差异仅来自哪项编译条件。
原工作流保持失败，后续32组native未执行，[失败归档复核器](evidence/http-future-layout-2026-11-04/phase-first/verify.py)保留两种布局、实际生产probe与通过的单项测试日志。

已将阶段测量入口限制在显式http-layout-probe功能和测试构建中，并由生产release的同一实际可执行文件测全部阶段和外层回调，再核对两者。
默认业务路径不启用此诊断功能。保留测试库探针，但不再把不同构建的布局直接等同；后续继续跑完整CI和32组native。

## 同一生产编译程序中的阶段结果

[Actions41782934201](https://github.com/Zelvik-Labs/edge/actions/runs/41782934201)通过并已复核实际release可执行文件、源码、11阶段及全部120块/1200万次构造测量，零steal。
在同一个启用http-layout-probe的production链接程序中，外层request_filter与阶段入口构造的外层frame均为5736字节，不再跨测试库比较。

| 阶段 | 未poll状态字节数 |
|---|---:|
| body_waf | 5600 |
| context | 2560 |
| early_response | 1456 |
| feature_policy | 1168 |
| lightweight_waf | 1992 |
| limits | 1912 |
| local_service | 2280 |
| protocol_policy | 2344 |
| rewrite | 688 |
| security_state | 32 |
| status_response | 1768 |

body_waf为5600字节，包含其嵌套异步状态；不能与其他阶段尺寸相加推算总帧。
request_filter构造/销毁中位27.31ns（26.97～53.63），其他回调5.76～10.33ns，与首次结论一致：单纯大frame构造并非几十微秒差距的解释。
未测首次poll、实际业务成本或调度成本，因此本轮没有给body_waf额外Box或跳过任何WAF。
归档13299399781：6,976,048字节、11文件，SHA-256 `5fd607276428e26f1826e8d03d94cb2581fcb66daf06835e8ff4eefebfd386db`。
[同构建冻结复核器](evidence/http-callback-construct-2026-11-04/phases/verify.py)保留完整证据。累计完整CI41782989939仍在执行32项native验证，诊断通过不代替功能CI。

累计[CI41782989939](https://github.com/Zelvik-Labs/edge/actions/runs/41782989939)现已完整通过并复核：1369库测试、16二进制测试、5项ignored、集成/vendor/check/Clippy、同生产编译方式的布局与32项native节点控制。
[归档和原始日志摘要](evidence/http-future-layout-2026-11-04/full-ci/ci.json)明确：native归档69文件全量校验，未包含节点实际ELF，属于正确性证据，不作为可执行文件字节身份或吞吐证明。
