# 原生 H3 持续连接周转验收（2026-11-04，香港时间）

最新公共字节门槛修正在独立 CI 完成 **262,144/262,144 次新建连接请求**，零错误，
没有最终准入拒绝、OOM 或 HIT 额外回源。单个进程的有效测量窗口为 **254.82 秒**，
支持短期持续周转修复，不等同于 24/72 小时稳定性验收。

[工作流 41692867829](https://github.com/Zelvik-Labs/edge/actions/runs/41692867829)成功。
驱动提交 `1911e52f0ff4835845e6d01a1bd5b65f8a5cab1c` 仅在
`562d49c1b1dc2f754acaa33f842bb266911fbcfb` 上增加持续测试入口，
`src`、`crates`、`Cargo.toml` 和 `Cargo.lock` 的 Git 对象完全相同，
见[运行时等价证明](evidence/h3-native-sustained-2026-11-04/runtime-equivalence.json)。

## 条件和结果

固定 1 代理 CPU、2 GiB、32 并发，两个独立客户端 CPU 和一个 H1 源站 CPU。
tokio-quiche-native/matched、地址验证 Off；H3/HIT/fresh、1 KiB 正文。
每请求强制新建连接、关闭 TLS 会话恢复；严格校验信任、实际 H3、完整正文和新建连接数。
保留原有按请求数计算的截止时间，没有增加内存或放宽错误判定。

| 指标 | 实测 |
|---|---:|
| 成功 / 失败 | 262,144 / 0 |
| 测量窗口 | 254.8154 秒 |
| 吞吐 | 1,028.76 请求/秒 |
| P99 | 41.056 ms |
| 代理 CPU / 成功请求 | 962.80 µs |
| 代理 CPU / 配额 | 99.05% |
| 客户端 CPU / 配额 | 66.59% |
| 测量期间额外回源 | 0 |
| 已记录最大 host steal | 0 |
| cgroup memory.events 各项 | 全部 0 |

该吞吐主要覆盖完整 TLS/QUIC 新建连接，不能与缓存命中复用 RPS 混比。
代理 CPU 已接近单核饱和；没有竞品或旧版在同一主机的持续对照，不发布跨主机提速倍率。

进程 RSS 仅保留窗口前后：Edge **160.74→182.74 MiB**，线程数均为 25。
控制器单独记录；cgroup 峰值为 168.26 MiB，与进程 RSS 计量范围不同，不能相加或要求二者相等。
没有连续 RSS、PSS 或 FD 采样，不据此证明无泄漏，也不把前后增加直接判为泄漏。

## 修复与此前失败

此前 `b30f0cf` 的正式短探针 1536/1536、完整矩阵 tokio-quiche 81/81 均通过，
但本地持续 262,144 请求仍有 1565 个错误。首次直接快照显示拒绝发生在更早的连接准入，
没有走到当时 transport reservation 内的释放确认逻辑。
本地测试还受到最高 43.03% host steal 干扰，不能将其所有超时都归于该分支。
完整失败保留于[准入诊断](performance-h3-native-admission-2026-11-04.md)。

`562d49c` 只在原生容量所有者仍存在且公共共享字节门槛失败时，
对足够的已退役未确认容量执行物理观测，并按同一硬门槛重试一次。
存活队列仍全额扣住容量，分类型配额、控制面及已建立流量保留额度不变。
完整特性 CI 的 1350+16 测试及三轮短探针另行通过。

## 复核

原始测量 artifact `13260558009`，5,002,309 B，SHA-256
`c8d130716c52c279c510b08ed6655ed0bc4b2bf8a8ea163b85c35cb87f86734b`。
实际二进制 artifact `13260278195`，21,011,035 B，SHA-256
`077c1d2e63d282e8c4fdee3b2478d0a6950e9301bc794135398057f969ce7f91`。
实际 Edge SHA-256 为 `7725842e7494ff4030d7666a3324638d71faa0efac91fbadc5513b4809193dd1`；
Edge、控制器、客户端均按实际字节与测量 manifest 对齐。
全部 39 个原始文件及 262,144 条传输已重算，包含前后进程 RSS、缓存计数和失败检查。
两 artifact 到期日为 2027-02-01 UTC；[元数据、摘要和复核脚本](evidence/h3-native-sustained-2026-11-04)已入库。

```bash
gh api repos/Zelvik-Labs/edge/actions/artifacts/13260558009/zip > /tmp/h3-native-sustained.zip
python3 docs/evidence/h3-native-sustained-2026-11-04/verify.py --archive /tmp/h3-native-sustained.zip
```

已修复：实测覆盖的持续新连接准入顺序问题。
仍存在：WAF/共享路径的其他性能成本、原生后端尚无全面领先证据。
设计限制：单进程约 4.25 分钟、HIT/小正文/fresh，尚无长时缓存替换、FD 或公网负载结论。
经审批的功能降级：无；预算、TLS、缓存和超时断言保持不变。
