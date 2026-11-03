# 最终独立协议、缓存及连接状态矩阵（2026-11-04，香港时间）

本轮默认生产路径的复测已完成并重算全部原始记录：Edge **180/180** 主矩阵场景通过，
另外 **54/54** UDP 批处理场景通过；Nginx 168/180、OpenResty 166/180。
竞品失败集中于 H3 多路复用，不能据此认定服务器本身有缺陷，也不能把失败当作零吞吐排名。
**H1/H2 仍有明显 CPU 差距；H3 吞吐受客户端限制，没有证据支持 Edge 已整体超过两者。**

测量提交 `a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a`，
[工作流 41665387055](https://github.com/Zelvik-Labs/edge/actions/runs/41665387055)。
使用未启用 h3-quiche 的默认 release 可执行文件；后续原生接收边界及诊断改动位于可选特性路径，
不改变这里被测的默认数据面。这不是 tokio-quiche 的成绩。

## 吞吐与 CPU

全部 **90 个协议/缓存/连接/正文组合**的两轮中位数及 P99/CPU/内存明细，见
[完整对比表](evidence/http-state-2026-11-04/verified-comparison.md)。
[594 条场景 CSV](evidence/http-state-2026-11-04/verified-cases.csv) 同时保留独立 RX 实验及失败。
以下摘录 1 KiB、HIT、reuse，CPU 为代理 cgroup 每成功请求的微秒：

| 协议 | Edge RPS | Nginx RPS | OpenResty RPS | Edge CPU µs | Nginx CPU µs | OpenResty CPU µs |
|---|---:|---:|---:|---:|---:|---:|
| H1 | 16,328 | 26,914 | 26,656 | 56.17 | 35.54 | 35.72 |
| H1 TLS | 13,329 | 23,303 | 23,962 | 69.09 | 39.72 | 38.67 |
| H2 | 13,707 | 20,031 | 20,858 | 70.44 | 45.59 | 43.52 |
| H3 | 9,168 | 9,478 | 9,074 | 98.22 | 77.54 | 77.54 |

H1/H2 的 Edge 代理核占用约 92%～97%，客户端约 68%～74%，每请求 CPU 仍较高。
H3 客户端核约 95%～97% 忙，吞吐接近不等于服务端容量相同；Edge 的 CPU/request 仍高约 27%。
不同 Actions runner 上的绝对数值不能与[前一份矩阵](performance-http-state-2026-11-03.md)
串成优化前后趋势。本轮没有同机旧版二进制对照，故不宣称两报告差额是代码收益或回退。

## 条件及内存口径

- H1/H1 TLS/H2/H3 × OFF/MISS/HIT × reuse/fresh，H2/H3 另有 multiplex；正文 1/32/128 KiB。
  三目标两轮，第二轮反转目标顺序。每格新进程、新缓存 tmpfs，目标与编译均不在测量主机上混跑。
- 代理 CPU 0，客户端 1/2，源站 3；代理限 1 CPU、2 GiB。源站统一 H1，客户端为原生 libcurl multi 8.14.1。
  Nginx 1.28.0、OpenResty 1.27.1.2-0-bookworm-fat，实际构建均含 H3。
- reuse 共 32 个槽；multiplex 为两个客户端各一条连接、16 并发流。fresh 明确新建并禁止复用连接，
  禁用 TLS 会话恢复，逐传输检查实际建连数与协议。
- reuse/OFF/HIT 计时 3 秒，MISS 为 1024 个唯一 URL，fresh 为 512 请求；315 个限量批次不足 1 秒。
  全部记录的 host steal 为零，但短窗口仍不适合细小百分比排名。
- 每次请求检查 TLS、实际协议和逐字节正文；测量前后复核 SHA、缓存响应头与源站计数。
  HIT 必须零回源，OFF/MISS 回源数必须对应成功请求，且没有新增 OOM kill。
- 关闭访问日志、压缩、业务 Lua、自定义 WAF 规则；Edge `metricItems=[]`，基础统计及真实认证控制器仍运行，
  控制器 CPU 包含在代理 cgroup。各产品缓存/TLS 实现不同；Nginx/OpenResty 使用 tmpfs 文件缓存，
  Edge 使用真实混合缓存。最大新正文集合约 128 MiB，没有测长期容量替换。

新增 [逐进程 RSS 报告](evidence/http-state-2026-11-04/verified-process-rss.md)，共 3760 个观测。
7 个超时场景未获得完整前后 RSS，明确保留为缺失。1 KiB/HIT/reuse 的 Edge 主进程结束 RSS
为 H1 154.58 MiB、H1 TLS 156.26 MiB、H2 156.10 MiB、H3 162.61 MiB。
它们不是峰值、PSS 或 WAF 优化前后差值。控制器与 Nginx/OpenResty 各进程 RSS 分别保留，
不把共享页重复相加；cgroup 内存峰值还包含文件页和 tmpfs，独立列示。

## 失败及 UDP 复测

Nginx 有 9 个 H3/multiplex 场景各出现 16 次传输错误，另 3 个 H3/MISS/multiplex 触发 90 秒场景上限。
OpenResty 对应为 10 个错误场景及 4 个超时场景。两轮任一场景失败，该格均不发布可比较中位数。
当前证据尚未区分服务器实现、OpenSSL/libcurl H3 客户端或互操作原因；不把它们归因于 Edge 的 quiche FIN 缺陷。

同一 Edge 二进制另测 batch=1/32、三轮交替顺序，54 格全部通过。
配对 HIT/reuse 的 RPS 中位变化为 **-0.70%**（范围 -1.18%～+0.17%），CPU/request 为 **+1.02%**；
HIT/multiplex 为 **+0.86%**（-1.56%～+1.08%），CPU/request **-2.59%**。
这没有重现首轮一致吞吐优势，**默认继续保持 batch=1**。短批 MISS/fresh 的 +7.51% 不能覆盖其他状态。
全部配对值在 [RX 原始配对摘要](evidence/http-state-2026-11-04/verified-rx-pairs.json)，
[RX 完整表](evidence/http-state-2026-11-04/verified-rx.md) 保留每格 CPU、P99 与瓶颈信息。

## 复核与交付

已重扫 594 个场景、6,606,638 个成功测量请求，重算 RPS、P99、连接数、错误分布、CPU/request、
回源计数和全部完整格中位数。568 格通过、19 格有流量错误、7 格超时，所有原件保留。
原始 artifact `13251357935`：51,163,176 B，SHA-256
`022bdf79a09a974ec13b9488cce6d063900ab6042ab888d1cb7d38eb57827b8b`，6361 个文件均已核验。

实际可执行文件独立 artifact `13250758275`：19,390,739 B，SHA-256
`ba422c56d099b1978e075d2ec8d3d39b344b55bd75f3b87639daf555a95b0478`。
Edge 文件 SHA-256 为 `291c5cae4857d20c3f945d99e01a909740bfe0e54687e5cef4e131b817812c25`；
已将 Edge、控制器及客户端实际字节与每个测量 manifest 对齐。两 artifact 当前到期日均为 2027-02-01 UTC。

大 ZIP 和二进制保留在工作区及 Actions，仓库保存元数据、逐文件摘要、全部结果和复核脚本：

```bash
gh api repos/Zelvik-Labs/edge/actions/artifacts/13251357935/zip > /tmp/http-state-final.zip
python3 docs/evidence/http-state-2026-11-04/verify.py --archive /tmp/http-state-final.zip
```

另下载二进制 artifact 并解包后，可用 `scripts/perf/http-state/verify.py <测量解包目录> --binaries <二进制解包目录>`
复核实际执行文件。此处所有报告数值均已完成该步骤。

已完成：最终默认数据面的全状态独立对比、实际二进制复核、进程 RSS 与 RX 重复验证。
仍存在：H1/H2 的 CPU 差距及竞品 H3/multiplex 互操作失败，未取得整体领先证据。
设计限制：短时云测试、H3 客户端饱和、不同缓存/TLS 实现，尚无 24/72 小时稳定性或公网条件结论。
经审批的功能降级：无；没有修改错误、超时、TLS 或缓存正确性判断。
