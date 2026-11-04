# 全局容量修正后的 H3 基线（2026-11-04，香港时间）

`b30f0cfa1cdaa044e295ee50c77ee1d9c09979b6` 的 243 场景矩阵中，共享 Quinn **81/81**、
原生 Quinn **80/81**、tokio-quiche **81/81** 通过。完整 CDN/cache 路径上的短场景准入问题已改善；
同版本后续持续周转仍失败，不能据此宣布长期稳定，详见[准入诊断](performance-h3-native-admission-2026-11-04.md)。
此报告早于 `562d49c` 将释放确认覆盖到公共字节门槛的修正。

[工作流 41685549261](https://github.com/Zelvik-Labs/edge/actions/runs/41685549261)
因原生 Quinn 一个真实错误返回失败。所有 243 格及错误保留，未将部分成功吞吐计入可比较中位数。

## 条件和结果

三个后端使用同一 feature-enabled 生产可执行文件、matched 配置、地址验证 Off、
1 代理 CPU/2 GiB、两个客户端 CPU、一个 H1 源站 CPU。逐场景独立进程与缓存，三轮轮换顺序。
覆盖 H3 × OFF/MISS/HIT × reuse/fresh/multiplex × 1/32/128 KiB。
完整正文、真实 HTTP/3、TLS 信任、连接行为和回源计数均参与验收。

| 后端 | 通过 / 场景 | 未通过 |
|---|---:|---|
| 共享 Quinn | 81/81 | 无 |
| 原生 Quinn | 80/81 | 轮次 1、128 KiB/HIT/fresh 中 1/512 请求失败 |
| 原生 tokio-quiche | 81/81 | 矩阵内无；另一次持续周转失败 |

原生 Quinn 失败为 curl 56、HTTP 0、OpenSSL QUIC protocol error，正文为零。
该错误与之前原生 Quinn 基线出现的同类错误一起保留，尚未定位到服务器或客户端，
不能归因为 tokio-quiche 的准入门槛或已修复 FIN 缺陷。

## 吞吐与 CPU

[完整 27 格比较表](evidence/h3-native-capacity-matrix-2026-11-04/verified-native.md)和
[243 条原始重算结果](evidence/h3-native-capacity-matrix-2026-11-04/verified-cases.csv)提供全部状态。
下表为全部通过的 HIT/reuse，单元格为三轮中位数 `RPS / CPU µs/成功请求`：

| 正文 | 共享 Quinn | 原生 Quinn | 原生 tokio-quiche |
|---|---:|---:|---:|
| 1 KiB | 6,984 / 131.41 | 7,078 / 127.19 | 4,718 / 196.40 |
| 32 KiB | 1,525 / 619.86 | 2,212 / 308.61 | 2,088 / 370.00 |
| 128 KiB | 490 / 1905.39 | 549 / 1007.89 | 558 / 1073.32 |

中大正文的原生 Quinn CPU 成本显著低于共享入口，支持继续梳理共享 UDP 路径的复制、调度及 offload。
本实验同时改变传输入口行为，不能将全部差额归于一个锁或一次复制。
tokio-quiche 在小对象明显更贵，在中大对象更接近原生 Quinn，尚无全面替换依据。
多格客户端 CPU 约为可用双核的 85%～96%；不同云 runner 的绝对 RPS 不作前后收益比较。

## 完整性与边界

独立重算 933,171 次成功传输，242 格通过、1 格流量错误、没有场景级超时。
37 个限量批次短于 1 秒；已记录窗口的最大 host steal 为零。
[972 个逐进程 RSS 观测](evidence/h3-native-capacity-matrix-2026-11-04/verified-process-rss.md)完整，
无缺失；RSS、cgroup 峰值、逻辑容量预留分开报告，没有相加或当作 WAF 前后差值。

原始 artifact `13259900948`，16,362,384 B，SHA-256
`24896113c70063fbc18b36fe3183602a2662c481e3646a599550acae7059785a`；
二进制 artifact `13258023268`，21,014,009 B，SHA-256
`5d818e2b336b3dda84def0fa35daed3094eef040e2e68b76ca7d14a3bb3be341`。
实际 Edge SHA-256 为 `c5babc3fc010e1c0175e0ad316b002529ca1617e1888de4d70f3a0af47bebd38`，
与所有测量 manifest 对齐。3065 个原始文件及实际客户端/控制器/Edge 字节全部核验。
产物到期日为 2027-02-01 UTC；[仓库证据](evidence/h3-native-capacity-matrix-2026-11-04)
保留元数据、逐文件摘要和可重算结果，大 ZIP 保留在工作区与 Actions。

```bash
gh api repos/Zelvik-Labs/edge/actions/artifacts/13259900948/zip > /tmp/h3-capacity-matrix.zip
python3 docs/evidence/h3-native-capacity-matrix-2026-11-04/verify.py --archive /tmp/h3-capacity-matrix.zip
```

已修复：本阶段 tokio-quiche 短矩阵覆盖的 workspace/释放窗口准入故障。
仍存在：同版本持续连接周转失败；原生 Quinn 一个独立 QUIC 错误。
设计限制：短测、部分客户端饱和、不同 TLS/offload；原生后端尚未覆盖全部生产语义。
经审批的功能降级：无；未提高内存、放宽超时或忽略失败。
