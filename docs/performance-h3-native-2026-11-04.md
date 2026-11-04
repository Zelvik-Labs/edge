# H3 三后端最终矩阵（2026-11-04，香港时间）

最新运行时 `562d49c1b1dc2f754acaa33f842bb266911fbcfb` 的完整矩阵已完成并独立重算：
共享 Quinn **81/81**，tokio-quiche **81/81** 通过；原生 Quinn **79/81**。
tokio-quiche 的缓存/复用/新连接准入验收通过，但小对象 CPU 成本仍更高，保持实验开关，默认不变。

[Actions 41692105912](https://github.com/Zelvik-Labs/edge/actions/runs/41692105912)
因原生 Quinn 两个真实传输错误正确返回失败。所有错误保留；两格不发布可比较中位数。
同运行时另通过[262,144 次持续新建连接](performance-h3-native-sustained-2026-11-04.md)，
以及 1350+16 个功能测试、三轮 1536 次短连接探针；这些验证不能相互替代。

## 同条件结果

同一个启用 h3-quiche 的生产节点切换三后端，统一 matched 配置、地址验证 Off、
1 代理 CPU/2 GiB、两个客户端 CPU、一个 H1 源站 CPU。三轮轮换顺序，每格重启节点和缓存。
覆盖 H3 × OFF/MISS/HIT × reuse/fresh/multiplex × 1/32/128 KiB。
直接进入相同 CDN/cache handler，保留完整正文、TLS、协议、缓存和回源检查。

[完整 27 格对照表](evidence/h3-native-final-2026-11-04/verified-native.md)、
[243 条场景 CSV](evidence/h3-native-final-2026-11-04/verified-cases.csv)包含全部重复与失败。
下表摘录全部通过的 HIT/reuse，单元格为三轮中位数 `RPS / CPU µs/成功请求`：

| 正文 | 共享 Quinn | 原生 Quinn | 原生 tokio-quiche |
|---|---:|---:|---:|
| 1 KiB | 11,162 / 81.28 | 11,904 / 74.33 | 7,802 / 115.79 |
| 32 KiB | 2,518 / 364.60 | 4,775 / 171.38 | 4,350 / 206.53 |
| 128 KiB | 827 / 1117.36 | 1,268 / 548.83 | 1,247 / 605.88 |

原生路径继续在中大正文下减少代理 CPU；tokio-quiche 小对象成本更高，不能说换库全面更快。
共享入口的两次用户态接收复制、队列交接及发送 GSO 差异已做[代码复查](h3-transport-backends.md)，
但各项差额尚未独立定量。多数命中格客户端约 88%～97% 忙，吞吐不是服务端极限容量。
此轮 matched 三后端和默认生产配置的跨产品矩阵是不同实验，不能混合排名。
不同 runner 的绝对 RPS 也不与前几轮相减，冒充同机代码收益。

## 两个独立错误

| 轮次 | 原生 Quinn 场景 | 成功 / 失败 | 客户端连接 ID |
|---|---|---:|---:|
| 1 | 32 KiB/HIT/fresh | 511 / 1 | 84 |
| 2 | 128 KiB/OFF/fresh | 511 / 1 | 247 |

二者都是 `curl_56/http_0/v_0`，OpenSSL `SSL routines::quic protocol error`，未收到正文。
本轮没有最终原生准入拒绝日志。错误与之前原生 Quinn 基线中的同类错误一起保留，
尚未区分服务器、客户端或内核 offload/互操作原因；不能归给 tokio-quiche 的已修复 FIN/容量缺陷。
后续需要带双方 qlog/报文和受控 offload 开关复现，当前不将原生 Quinn 提升为默认。

## 内存、完整性和复核

共 243 场景：241 通过、2 流量错误、零场景级超时；重算 1,487,834 个成功传输及两个失败传输。
92 个限量批次不足 1 秒，记录窗口的最大 host steal 为零。
[972 个逐进程 RSS 观测](evidence/h3-native-final-2026-11-04/verified-process-rss.md)齐全，无缺失。
1 KiB/HIT/reuse 结束时 Edge RSS 中位数为共享 Quinn 168.64 MiB、原生 Quinn 169.95 MiB、
tokio-quiche 157.16 MiB；这是结束快照，不是峰值、PSS、逻辑预留或 WAF 优化前后差值。

原始 artifact `13262858779`，26,591,423 B，SHA-256
`cea6a1cc546c8f30b84253ad96922697ec4b3f72a6d8101c6fc88370fa3a3ddc`。
实际二进制 artifact `13262973633`，21,011,074 B，SHA-256
`f72c9b8c25cbb9c1a5b003a028a22e241116c17996fe1803df521c1718792f24`。
实际 Edge SHA-256 为 `cfb8e3a83caa6b39b3a6aa85b7af3af8e8db26ea919c43408d99ca9cc9b466f3`。
3065 个原始文件及 Edge/控制器/客户端实际字节均已核验，与全部测量 manifest 一致。
两 artifact 到期日为 2027-02-01 UTC；大 ZIP 保留于工作区与 Actions，
[仓库证据](evidence/h3-native-final-2026-11-04)包含摘要、全部结果和复核脚本。

```bash
gh api repos/Zelvik-Labs/edge/actions/artifacts/13262858779/zip > /tmp/h3-native-final.zip
python3 docs/evidence/h3-native-final-2026-11-04/verify.py --archive /tmp/h3-native-final.zip
```

另解包二进制 artifact 后，向复核命令增加 `--binaries <二进制目录>` 可重新对齐实际文件。

已修复：本轮覆盖的 tokio-quiche CDN/cache、FIN、接收上限和连接容量准入问题。
仍存在：两个原生 Quinn QUIC 错误、小对象 quiche 成本及共享路径的 CPU 差距。
设计限制：短时云环境、部分客户端饱和、TLS/offload 不同；无 24/72 小时和完整生产策略对等结论。
经审批的功能降级：无；未放宽超时、资源、TLS、缓存或错误判定。
