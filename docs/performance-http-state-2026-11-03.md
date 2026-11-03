# 独立协议、缓存和连接状态对比（2026-11-03，香港时间）

第一套生产节点矩阵已经完成。Edge 的 180 个场景全部通过；同机独立运行的
Nginx 有 7 个 H3 多路复用场景失败，OpenResty 有 1 个。它们的失败保留为失败，
不填 0 RPS、不参与完整单元格的吞吐比较，也不据此断言服务器存在缺陷。
另有 54 个 Edge UDP 批处理实验全部通过。

[工作流 41653073592](https://github.com/Zelvik-Labs/edge/actions/runs/41653073592)
源提交为 `cd51c20d193418d225c2777c3fd4d39defdf9522`，工作流因上述失败正确返回失败。
这份报告是该次测量的完整记录，不代表后续 tokio-quiche 功能提交的测量结果。

## 结果

完整的 **90 个协议/缓存/连接/正文组合**，每目标两轮，见
[吞吐、P99、CPU 和内存明细](evidence/http-state-2026-11-03/verified-comparison.md)。
[594 条场景记录 CSV](evidence/http-state-2026-11-03/verified-cases.csv)
同时包含 UDP 实验及所有失败。

以下仅摘录 1 KiB、缓存命中、普通连接复用的两轮中位数，完整表不省略其他场景：

| 协议 | Edge RPS | Nginx RPS | OpenResty RPS | Edge CPU µs/成功请求 | Nginx | OpenResty |
|---|---:|---:|---:|---:|---:|---:|
| H1 | 42,661 | 73,866 | 74,635 | 21.30 | 12.83 | 12.41 |
| H1 TLS | 37,543 | 64,444 | 70,133 | 24.15 | 14.17 | 12.86 |
| H2 | 33,025 | 56,738 | 60,210 | 29.22 | 15.98 | 14.47 |
| H3 | 21,004 | 17,521 | 19,198 | 42.52 | 32.79 | 29.16 |

H1/H2 的 Edge 代理核占用约 91%～97%，每请求 CPU 明显较高，仍有真实优化空间。
H3 的两个压测核平均占用约 96%～97%，因此这组 H3 吞吐**不能用来推断服务端容量上限**；
Edge 的 H3 CPU/request 仍较高。新建 TLS 连接与缓存复用呈现不同排序，不能混成一个“快多少”。

## 条件及验证

- 各目标、各场景依次运行；每格重新创建代理进程和缓存 tmpfs。两轮目标顺序反转。
- 实际 CPU 分配：代理 0，客户端 1、2，源站 3；代理限 1 CPU、2 GiB，源站使用同一 H1 配置。
- Edge 为 release 生产启动路径及已认证测试控制器；Nginx 1.28.0，OpenResty 1.27.1.2-0-bookworm-fat。
  两个打包服务器的实际构建均支持 H3。各镜像 ID、版本和二进制摘要见
  [summary.json](evidence/http-state-2026-11-03/summary.json)。
- H1/H1 TLS/H2/H3 × OFF/MISS/HIT × fresh/reuse；H2/H3 另测 multiplex；正文 1/32/128 KiB。
  reuse 共 32 个槽位；multiplex 为两个客户端各一条连接、16 个并发流。
  fresh 使用 `CURLOPT_FRESH_CONNECT`，关闭 TLS 会话恢复，逐请求校验确实新建连接。
- reuse/HIT 与 OFF 计时 3 秒；MISS 为 1,024 个唯一 URL，fresh 为 512 请求。
  全套有 348 个限量批次不足 1 秒；它们受到进程启动、握手与计时粒度影响，适合作为快速状态对照，
  不适合细小百分比排名或替代长时间稳态容量测试。
- 严格验证证书、实际协议、逐字节正文；场景前后再核对正文 SHA、缓存响应头与源站计数。
  HIT 必须零回源，OFF/MISS 回源数必须对应成功请求。失败不会被降为缓存未命中或协议回退。
- 关闭访问日志、压缩、业务 Lua 和自定义 WAF 规则；Edge `metricItems=[]`。
  Edge 基础统计及真实控制器仍运行，其 CPU 包含在代理 cgroup 中。
  Nginx/OpenResty 使用 tmpfs 文件缓存，Edge 使用实际混合缓存；各自内部缓存算法、预算分配和 TLS 实现并不相同。
  本次每个新缓存的最大正文集合约 128 MiB，没有测缓存容量耗尽后的替换稳态。

`verify.py` 已重新扫描原始逐传输记录，核验 594 个场景、16,283,624 个成功测量请求，
重算 RPS、P99、连接数、CPU/request、宿主机 steal 和完整单元格中位数。
全部已记录测量窗口的 host steal 为零。cgroup `memory.peak` 含文件页、tmpfs 和控制器，
**不能称为 Edge 进程 RSS**；该旧版工作流未采集逐场景进程 RSS，后续版本已补上。

## 失败及证据

3 个 Nginx H3/MISS/multiplex 场景有一侧客户端连续 10 秒请求超时，最终触发 90 秒场景上限。
其余 4 个 Nginx、1 个 OpenResty 场景各有 16 个传输错误。
当前证据不能区分服务器、libcurl HTTP3 实现和互操作原因；不把它们归因于 Edge 的 quiche FIN 缺陷。
这些单元格不发布可比较的中位数，部分成功请求和错误日志均保留。

原始 artifact `13242628146`：107,883,975 字节，SHA-256
`f1c03944e2e4333a2e73472bbe45733c49ff4bed6e49db91d529599385130437`。
已下载并核验全部 6,368 个文件（解包 231,545,180 字节）；大 ZIP 保存在工作区及上述 Actions artifact，
不重复放入 Git。仓库保留原始 artifact 元数据、逐文件摘要、重算结果和复核脚本。
该 artifact 当前到期日为 2027-02-01 UTC。可先下载，再在仓库根目录复核：

```bash
gh api repos/Zelvik-Labs/edge/actions/artifacts/13242628146/zip > /tmp/http-state.zip
python3 docs/evidence/http-state-2026-11-03/verify.py --archive /tmp/http-state.zip
```

客户端实际可执行文件在该 artifact 内，摘要已核验；旧工作流仅记录了 Edge/控制器的运行时文件摘要，
没有上传它们的可执行文件，因此不声称已独立复核这两个二进制。后续工作流已增加独立二进制 artifact。

已解决：缺少跨协议、缓存、连接状态的独立比较和逐请求复核。
仍待验证：24/72 小时稳定性、受控公网 RTT/丢包、长期缓存替换、业务 Lua/WAF、服务端极限容量。
设计限制：短时云主机、固定并发、同机 Docker 网络及不同 TLS/缓存实现。
经审批的功能降级：无。
